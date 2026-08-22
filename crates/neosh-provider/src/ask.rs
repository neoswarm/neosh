//! Asking a person a *question*, from inside a driver.
//!
//! # Why this is not the permission path
//!
//! [`crate::approval`] carries "may this happen", and every answer to that is a yes or a no. This
//! carries "which of these", and the two are not the same shape or the same decision:
//!
//! * a permission request has one question; this has a list of them, answered in one sitting;
//! * a permission option is a decision *about the request*; a question's options are the subject
//!   matter, and one of them may be several, and one of them may be something nobody listed;
//! * a permission request is answerable by **policy** — that is the whole point of
//!   [`neosh_agent::PermissionLayer`] — and a question is not. `full access` means "do not ask me
//!   before writing files". It cannot mean "and pick a database for me".
//!
//! That last one is why routing questions through the permission path did not merely read badly,
//! it silently broke: the configured default is full access, so the layer answered `Allow` before
//! anybody was asked, the driver sent back a bare yes, and `claude` replied to its own model with
//! *"The user did not answer the questions."* — a turn that asked you something, never showed it to
//! you, and then told the agent you had ignored it. See ADR 0043.
//!
//! # The shape
//!
//! As thin as the approval trait next door, and for the same reason: a driver has no business
//! knowing whether a picker, a panel or a test closure is on the other end.

use std::path::PathBuf;

use neosh_proto::{QuestionAnswer, SessionId, UserQuestion};

/// A question an agent driver is blocked on the answer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionRequest {
    /// Everything being asked, in the order the agent asked it. Never empty — a driver with
    /// nothing to ask does not raise one of these.
    pub questions: Vec<UserQuestion>,
    /// Which conversation is asking. Several turns run at once and only one is on screen.
    pub conversation: SessionId,
    /// Where that turn is running.
    pub cwd: PathBuf,
}

/// What came back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionAnswers {
    /// One entry per question, keyed by [`UserQuestion::question`].
    ///
    /// A caller may answer *some* of them: partial answers are the honest report of a panel that
    /// was dismissed half-way, and the agent is told which ones it got rather than none of them.
    Answered(Vec<QuestionAnswer>),
    /// Nobody answered — dismissed, interrupted, or nothing listening.
    ///
    /// The reason is said to the *agent*, not logged: it is the difference between "the user
    /// dismissed this" and "there was nobody at the keyboard", and which of those happened changes
    /// what the model should do next.
    Cancelled { reason: String },
}

impl QuestionAnswers {
    /// The refusal a driver gives when there is no one to ask.
    pub fn nobody() -> Self {
        Self::Cancelled { reason: "There is no one at this workspace to answer.".into() }
    }

    /// The refusal a driver gives when the person walked away from the question.
    pub fn dismissed() -> Self {
        Self::Cancelled { reason: Self::dismissed_reason().into() }
    }

    /// What the agent is told when a question was shown and not answered.
    ///
    /// Its own function because two places say it: the driver, when the panel is dismissed, and
    /// the reply builder, when the answers that came back are all empty — which is the same event
    /// arriving by a different route.
    pub fn dismissed_reason() -> &'static str {
        "The user dismissed the question without answering."
    }
}

/// Somebody who can answer a [`QuestionRequest`].
///
/// Fail closed is the caller's job, as with [`crate::approval::PermissionAsker`] — except that here
/// "closed" is [`QuestionAnswers::Cancelled`], which is a *sentence the agent can act on* rather
/// than an error. A question nobody answered is an ordinary outcome; a question nobody was shown
/// is the bug.
#[async_trait::async_trait]
pub trait QuestionAsker: Send + Sync + std::fmt::Debug {
    async fn ask(&self, request: QuestionRequest) -> QuestionAnswers;
}
