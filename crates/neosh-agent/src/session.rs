//! Conversation state.

use std::path::PathBuf;

use neosh_proto::{
    ContentBlock, Message, ModelSelection, PermissionMode, Role, SessionId, SessionInfo, ToolCallId,
    ToolResult, TurnId, Usage,
};

#[derive(Debug, Clone)]
pub struct Session {
    pub id: SessionId,
    pub cwd: PathBuf,
    pub messages: Vec<Message>,
    pub selection: Option<ModelSelection>,
    pub system: Option<String>,
    /// Running total across the session, for budget display.
    pub usage: Usage,
    /// The prompt size of the most recent request: how full the window actually is.
    pub context_tokens: u64,
    /// What the driver says the window is. See [`neosh_proto::SessionInfo::context_window`].
    pub context_window: Option<u64>,
    pub active_turn: Option<TurnId>,
    /// Things said to *this* conversation while its turn was running, waiting for a gap to be
    /// taken in.
    ///
    /// Per conversation rather than per process: several turns can be in flight at once, and a
    /// single queue would hand what you typed here to whichever one reached a gap first.
    pub steering: Vec<String>,
    /// When the running turn started, in seconds since the epoch. Stamped by whoever starts the
    /// turn, for the same reason `created_at` is: this type does not read a clock.
    pub turn_started_at: Option<i64>,
    /// What the user, or a generated title, calls this conversation. `None` until something names
    /// it — a list then falls back to the first message, which is a better placeholder than
    /// "Untitled" because it says what the conversation is about.
    pub title: Option<String>,
    /// Seconds since the epoch. Set by whoever creates and touches the session, not read from a
    /// clock in here, so this type stays deterministic and testable.
    pub created_at: i64,
    pub updated_at: i64,
    /// Put away, not thrown away: still on disk, still openable, just not in the everyday list.
    pub archived: bool,
    /// When it was put away, in seconds since the epoch. Set by whoever archives it, for the same
    /// reason `created_at` is: this type does not read a clock.
    pub archived_at: Option<i64>,
    /// What this conversation lets the agent do without asking, when it has been told.
    ///
    /// See [`neosh_proto::SessionInfo::permission_mode`]. `None` is not a mode — it means nobody
    /// has chosen one here and the configured default applies, which is what makes changing that
    /// default in `config.toml` reach every conversation that has not overridden it.
    pub permission_mode: Option<PermissionMode>,
    /// What the driver calls this conversation, kept so a later process can pick it up.
    ///
    /// See [`neosh_proto::TurnRequest::resume`]. Held here rather than in the driver because the
    /// driver is a process and this is not: the whole point of the value is that it survives one.
    pub resume: Option<String>,
}

impl Session {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            id: SessionId::new(),
            cwd: cwd.into(),
            messages: Vec::new(),
            selection: None,
            system: None,
            usage: Usage::default(),
            context_tokens: 0,
            context_window: None,
            active_turn: None,
            steering: Vec::new(),
            turn_started_at: None,
            title: None,
            created_at: 0,
            updated_at: 0,
            archived: false,
            archived_at: None,
            permission_mode: None,
            resume: None,
        }
    }

    /// A label for a list: the title if there is one, else the opening of the first thing the user
    /// said, else a placeholder.
    pub fn label(&self) -> String {
        if let Some(t) = self.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            return t.to_string();
        }
        let first = self.messages.iter().find(|m| m.role == Role::User).and_then(|m| {
            m.content.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
        });
        match first.map(str::trim).filter(|t| !t.is_empty()) {
            // Truncated by *character*, not byte: slicing a multi-byte boundary would panic, and
            // the frontend measures display width anyway.
            Some(text) => {
                let mut out: String = text.lines().next().unwrap_or(text).chars().take(48).collect();
                if out.chars().count() < text.chars().count() {
                    out.push('\u{2026}');
                }
                out
            }
            None => "New conversation".to_string(),
        }
    }

    pub fn push_user_text(&mut self, text: impl Into<String>) {
        self.messages
            .push(Message { role: Role::User, content: vec![ContentBlock::Text { text: text.into() }] });
    }

    pub fn push_assistant(&mut self, msg: Message) {
        self.push_message(msg);
    }

    /// Record a message a turn produced, whichever role it belongs to.
    ///
    /// A driver that runs its own loop produces both — assistant, then the tool results as a user
    /// message, then assistant again — so "the assistant message" is not the shape of what comes
    /// back any more.
    pub fn push_message(&mut self, msg: Message) {
        // A turn that produced nothing is not worth replaying, and some providers reject an empty
        // content array outright.
        if !msg.content.is_empty() {
            self.messages.push(msg);
        }
    }

    /// Tool results go back as a single user message, matching the wire model of every provider we
    /// target. Splitting them across messages teaches models to stop making parallel calls.
    pub fn push_tool_results(&mut self, results: Vec<(ToolCallId, ToolResult)>) {
        if results.is_empty() {
            return;
        }
        self.messages.push(Message {
            role: Role::User,
            content: results
                .into_iter()
                .map(|(id, r)| ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: r.content,
                    is_error: r.is_error,
                })
                .collect(),
        });
    }

    /// What the driver says is in its window, and how big the window is.
    ///
    /// Believed over anything derived from usage, and it is not a close call. A prompt size is the
    /// tokens *this request* sent; an agent driver holds the conversation on its own side, decides
    /// for itself what to keep and when to compact, and is the only thing that can answer "how
    /// full is it". Where it does answer, guessing alongside it would be inventing a second number
    /// for a question that already has one.
    pub fn set_context(&mut self, used: u64, window: u64) {
        self.context_tokens = used;
        self.context_window = (window > 0).then_some(window);
    }

    pub fn add_usage(&mut self, u: &Usage) {
        // The most recent request's prompt is what fills the window. Replaced rather than summed:
        // adding them would report a conversation as longer than any request it ever made.
        //
        // Skipped entirely once a driver has told us: this is the estimate, and it is wrong in the
        // direction that matters — it counts one request rather than the conversation the agent is
        // holding, which for a CLI that compacts on its own is not the same thing at all.
        let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
        if prompt > 0 && self.context_window.is_none() {
            self.context_tokens = prompt;
        }
        self.usage.input_tokens += u.input_tokens;
        self.usage.output_tokens += u.output_tokens;
        self.usage.cache_read_tokens += u.cache_read_tokens;
        self.usage.cache_write_tokens += u.cache_write_tokens;
        self.usage.thinking_tokens += u.thinking_tokens;
    }

    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            cwd: self.cwd.display().to_string(),
            // Filled in by the host, which is the only thing that has looked at the repository.
            // The leaf directory is the honest fallback, and what every caller used before.
            project: self
                .cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.cwd.display().to_string()),
            message_count: self.messages.len() as u32,
            active_turn: self.active_turn.clone(),
            turn_started_at: self.turn_started_at,
            title: self.title.clone(),
            label: self.label(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            usage: self.usage,
            context_tokens: self.context_tokens,
            context_window: self.context_window,
            // Filled in by the store, which is the only thing that knows.
            is_active: false,
            archived: self.archived,
            archived_at: self.archived_at,
            permission_mode: self.permission_mode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_results_are_batched_into_one_user_message() {
        let mut s = Session::new("/tmp");
        s.push_tool_results(vec![
            (ToolCallId("a".into()), ToolResult::ok("1")),
            (ToolCallId("b".into()), ToolResult::error("boom")),
        ]);
        assert_eq!(s.messages.len(), 1, "splitting these suppresses parallel tool use");
        assert_eq!(s.messages[0].role, Role::User);
        assert_eq!(s.messages[0].content.len(), 2);
        assert!(matches!(
            &s.messages[0].content[1],
            ContentBlock::ToolResult { is_error: true, .. }
        ));
    }

    #[test]
    fn empty_assistant_messages_are_not_recorded() {
        let mut s = Session::new("/tmp");
        s.push_assistant(Message { role: Role::Assistant, content: vec![] });
        assert!(s.messages.is_empty());
    }

    #[test]
    fn usage_accumulates_across_turns() {
        let mut s = Session::new("/tmp");
        s.add_usage(&Usage { input_tokens: 10, output_tokens: 5, ..Default::default() });
        s.add_usage(&Usage { input_tokens: 7, output_tokens: 3, ..Default::default() });
        assert_eq!(s.usage.input_tokens, 17);
        assert_eq!(s.usage.output_tokens, 8);
    }
}
