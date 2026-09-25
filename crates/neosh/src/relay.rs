//! A question or permission prompt, offered to every machine watching the conversation it is in.
//!
//! An agent that stops to ask something stops in front of whoever can answer — and a conversation
//! driven from another computer is one whose person is at *that* computer. The owner's own panel
//! still opens (somebody may be sitting at it), and the same prompt goes out on the conversation's
//! stream; the first answer from either place is the answer, and the other place is told to take
//! its panel down. Nothing here decides anything: policy runs first exactly as it always did, and
//! only a request that would have reached a person here is offered to one somewhere else.
//!
//! Two wrappers stand in front of the drivers' askers, and a channel carries what they see to the
//! host loop, which is the only thing that can put an event on the wire or find the conversation a
//! machine's answer is about.

use std::sync::Arc;

use neosh_proto::{Capability, PermissionOption, QuestionAnswer, SessionId, UserQuestion};
use neosh_provider::approval::{PermissionAnswer, PermissionAsker, PermissionRequest};
use neosh_provider::ask::{QuestionAnswers, QuestionAsker, QuestionRequest};
use tokio::sync::{mpsc, oneshot};

/// What is being asked.
#[derive(Debug, Clone)]
pub enum Offer {
    Question(Vec<UserQuestion>),
    Permission { title: Option<String>, capability: Capability, options: Vec<PermissionOption> },
}

/// What came back from another machine.
#[derive(Debug)]
pub enum Reply {
    /// `None` is "dismissed", which the agent is told in those words.
    Answers(Option<Vec<QuestionAnswer>>),
    Approve(PermissionAnswer),
}

/// What the wrappers — and the tasks that ask on another machine's behalf — tell the host loop.
#[derive(Debug)]
pub enum Relay {
    /// A prompt opened on this machine. Offer it to the watchers, and keep `reply` for the answer.
    Opened { id: String, session: SessionId, offer: Offer, reply: oneshot::Sender<Reply> },
    /// It is not waiting any more. `elsewhere` when another machine's answer won, which means the
    /// panel here is still up over a question that has gone.
    Closed { id: String, session: SessionId, offer: Offer, elsewhere: bool },
    /// Somebody at this machine answered a prompt that belongs to another one's conversation.
    Answered { id: String, reply: Reply },
}

/// The questions half. Wraps whatever answers questions here.
#[derive(Debug)]
pub struct RelayQuestioner {
    pub inner: Arc<dyn QuestionAsker>,
    pub tx: mpsc::UnboundedSender<Relay>,
}

#[async_trait::async_trait]
impl QuestionAsker for RelayQuestioner {
    async fn ask(&self, request: QuestionRequest) -> QuestionAnswers {
        let id = uuid::Uuid::new_v4().to_string();
        let session = request.conversation.clone();
        let offer = Offer::Question(request.questions.clone());
        let (reply, answered) = oneshot::channel();
        // A workspace that has stopped has no loop to hear this, and nothing to offer it to.
        if self
            .tx
            .send(Relay::Opened { id: id.clone(), session: session.clone(), offer: offer.clone(), reply })
            .is_err()
        {
            return self.inner.ask(request).await;
        }
        let here = self.inner.ask(request);
        tokio::pin!(here);
        let (answer, elsewhere) = tokio::select! {
            a = &mut here => (a, false),
            Ok(Reply::Answers(a)) = answered => (match a {
                Some(a) if !a.is_empty() => QuestionAnswers::Answered(a),
                _ => QuestionAnswers::dismissed(),
            }, true),
        };
        let _ = self.tx.send(Relay::Closed { id, session, offer, elsewhere });
        answer
    }
}

/// The permissions half. Concrete rather than `dyn`, because it has to ask policy first: a request
/// full access says yes to in a microsecond is not one to put on another machine's screen.
#[derive(Debug)]
pub struct RelayAsker {
    pub inner: Arc<neosh_agent::DriverAsker>,
    pub tx: mpsc::UnboundedSender<Relay>,
}

#[async_trait::async_trait]
impl PermissionAsker for RelayAsker {
    async fn ask(&self, request: PermissionRequest) -> PermissionAnswer {
        if let Some(decided) = self.inner.policy(&request) {
            return decided;
        }
        // A prompt raised outside any conversation belongs to whoever is here.
        let Some(session) = request.conversation.clone() else {
            return self.inner.prompt(request).await;
        };
        let id = uuid::Uuid::new_v4().to_string();
        let offer = Offer::Permission {
            title: Some(request.title.clone()).filter(|t| !t.is_empty()),
            capability: request.capability.clone(),
            options: request.options.clone(),
        };
        let (reply, answered) = oneshot::channel();
        if self
            .tx
            .send(Relay::Opened { id: id.clone(), session: session.clone(), offer: offer.clone(), reply })
            .is_err()
        {
            return self.inner.prompt(request).await;
        }
        let here = self.inner.prompt(request);
        tokio::pin!(here);
        let (answer, elsewhere) = tokio::select! {
            a = &mut here => (a, false),
            Ok(Reply::Approve(a)) = answered => (a, true),
        };
        let _ = self.tx.send(Relay::Closed { id, session, offer, elsewhere });
        answer
    }
}

/// What a permission prompt says when the agent gave it no sentence of its own — the approvals
/// plugin's wording, so a prompt reads the same on either machine.
pub fn describe(c: &Capability) -> String {
    match c {
        Capability::ReadFile { path } => format!("Read {path}?"),
        Capability::WriteFile { path } => format!("Write {path}?"),
        Capability::Exec { command } => format!("Run {command}?"),
        Capability::Network { host } => format!("Connect to {host}?"),
    }
}

/// The plugin event that takes a prompt's panel down, because it was answered somewhere else.
///
/// Keyed the way each panel already keys what it is showing: a question by its text — which is
/// what the agent looks an answer up under — and a permission prompt by its sentence.
pub fn withdrawn(session: &SessionId, offer: &Offer) -> serde_json::Value {
    match offer {
        Offer::Question(qs) => serde_json::json!({
            "session": session,
            "kind": "question",
            "questions": qs.iter().map(|q| q.question.clone()).collect::<Vec<_>>(),
        }),
        Offer::Permission { title, capability, .. } => serde_json::json!({
            "session": session,
            "kind": "permission",
            "title": title.clone().unwrap_or_else(|| describe(capability)),
        }),
    }
}
