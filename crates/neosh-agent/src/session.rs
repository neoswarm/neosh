//! Conversation state.

use std::path::PathBuf;

use neosh_proto::{
    ContentBlock, Message, ModelSelection, Role, SessionId, SessionInfo, ToolCallId, ToolResult,
    TurnId, Usage,
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
    pub active_turn: Option<TurnId>,
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
            active_turn: None,
            turn_started_at: None,
            title: None,
            created_at: 0,
            updated_at: 0,
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
        // An assistant turn that produced nothing is not worth replaying, and some providers
        // reject an empty content array outright.
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

    pub fn add_usage(&mut self, u: &Usage) {
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
            message_count: self.messages.len() as u32,
            active_turn: self.active_turn.clone(),
            turn_started_at: self.turn_started_at,
            title: self.title.clone(),
            label: self.label(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            usage: self.usage,
            // Filled in by the store, which is the only thing that knows.
            is_active: false,
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
