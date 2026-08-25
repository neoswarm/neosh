//! Conversation state.

use std::path::PathBuf;

use neosh_proto::{
    ContentBlock, Message, ModelSelection, PermissionMode, Role, SessionId, SessionInfo, ToolCallId,
    ToolResult, TurnId, Usage,
};

/// One thing somebody said: the words, and anything they attached to them.
///
/// A pair rather than two arguments everywhere, because they travel together the whole way — into
/// the queue, out of it, through the turn, into the message. Almost every one of these has an
/// empty `images`, which is exactly why it is a field on the thing that is said rather than a
/// second queue running alongside the first and getting out of step with it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    /// Already [`ContentBlock`]s, because that is what they become and there is nothing in between
    /// for them to be.
    pub images: Vec<ContentBlock>,
}

impl Prompt {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), images: Vec::new() }
    }

    /// The message this is, images first.
    ///
    /// Before the words on purpose: "what is wrong with this?" reads as a question about the
    /// picture when the picture is what the sentence is pointing at, and every provider we target
    /// passes block order through untouched.
    ///
    /// An empty text block is kept when it is *all* there is, because a message with no content at
    /// all is one a driver would skip past to re-ask the question before it. It is dropped when
    /// there is a picture to carry the message instead — several providers reject an empty text
    /// block outright, and a picture on its own is a perfectly good thing to send.
    pub fn blocks(&self) -> Vec<ContentBlock> {
        let mut out = self.images.clone();
        if out.is_empty() || !self.text.is_empty() {
            out.push(ContentBlock::Text { text: self.text.clone() });
        }
        out
    }
}

impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

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
    /// Whether a driver has ever answered "how full is it" for this conversation.
    ///
    /// Not the same question as "is the window known", and spelling it as
    /// `context_window.is_some()` was one field standing in for two facts. `Activity::Compacted`
    /// carries `post_tokens` and no window — a driver answering the first question and saying
    /// nothing about the second — so applying it on a conversation whose window nobody had
    /// reported left this reading as *nobody has told us*, which turned the estimate in
    /// [`Self::add_usage`] back on. The next `TurnEnded` then set the meter to the prompt size of
    /// the request that did the compacting, which is the whole pre-compaction conversation:
    /// `/compact` moved the meter and then, a moment later, moved it back.
    pub context_counted: bool,
    pub active_turn: Option<TurnId>,
    /// Things said to *this* conversation while its turn was running, waiting for a gap to be
    /// taken in.
    ///
    /// Per conversation rather than per process: several turns can be in flight at once, and a
    /// single queue would hand what you typed here to whichever one reached a gap first.
    pub steering: Vec<Prompt>,
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
    /// A turn ended here while you were somewhere else, and you have not been back since.
    ///
    /// See [`neosh_proto::SessionInfo::unread`]. Held on the conversation rather than worked out
    /// by whoever draws a list, because "have you seen this" is a fact about the conversation and
    /// there is more than one list: the panel, a status segment and a picker all have to agree,
    /// and a second panel deriving it from timestamps would disagree with the first.
    pub unread: bool,
    /// A turn was in flight here when this conversation was last written to disk.
    ///
    /// Seconds since the epoch, stamped when the turn began. Persisted, and that is the entire
    /// point: nothing running in a process can tell you that a *previous* process died, so the only
    /// way to know a turn was cut off is to have written down that one had started and to find the
    /// note still there. Cleared when the turn ends, however it ends — an interrupt and an error
    /// are both endings, and neither is what this is for.
    pub running_since: Option<i64>,
    /// The workspace stopped while a turn was running here.
    ///
    /// See [`neosh_proto::SessionInfo::interrupted`]. Derived once, where it can be: at load, from
    /// a `running_since` that outlived the process that set it.
    pub interrupted: bool,
    /// What is still running here that no turn is waiting for.
    ///
    /// See [`neosh_proto::SessionInfo::background`]. Held on the conversation for the same reason
    /// `unread` is — several lists have to agree about it — and, unlike `unread` and unlike
    /// `interrupted` above, deliberately *not* written down. The two are opposite halves of one
    /// question and want opposite treatment: a turn cut off by a dead process can only be known
    /// from a note that outlived it, and a background shell can only be known from a driver that is
    /// still up to report it. Persisting this would mean a workspace coming back reporting a shell
    /// that died with it, forever.
    pub background: Vec<neosh_proto::BackgroundTask>,
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
    /// A place to type, minted because there was nowhere else — not a conversation you started.
    ///
    /// The store always has an active session: there has to be somewhere for the next message to
    /// go. That invariant is what used to make "the last session cannot be closed" a rule, and a
    /// workspace you have just cleared out is exactly when you want the opposite. So closing or
    /// archiving the last one lands you in one of these instead: it is the active session, the
    /// composer types into it and the transcript shows the welcome — but it is not in
    /// [`list`](crate::store::SessionStore::list) and it is never written to disk, because nobody
    /// asked for it and an empty workspace should look empty.
    ///
    /// It stops being one the moment anything is said in it, which is the only thing that makes a
    /// conversation a conversation. From then on it is an ordinary session: it appears in the
    /// panel, in its project, and is saved with the rest.
    pub ephemeral: bool,
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
            context_counted: false,
            active_turn: None,
            steering: Vec::new(),
            turn_started_at: None,
            title: None,
            created_at: 0,
            updated_at: 0,
            unread: false,
            running_since: None,
            interrupted: false,
            background: Vec::new(),
            archived: false,
            archived_at: None,
            permission_mode: None,
            resume: None,
            ephemeral: false,
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
            //
            // Generously, and deliberately more generously than any list will draw. This is a value
            // that is *forwarded*, so cutting it here cuts it for everybody — and a panel that can
            // unfold the row under the cursor to show the rest of a title cannot show a rest that
            // was thrown away three crates ago. Every consumer clips to its own column; the bound
            // here is only so that a pasted wall of text does not become a label.
            Some(text) => {
                let mut out: String = text.lines().next().unwrap_or(text).chars().take(160).collect();
                if out.chars().count() < text.chars().count() {
                    out.push('\u{2026}');
                }
                out
            }
            None => "New conversation".to_string(),
        }
    }

    pub fn push_user_text(&mut self, text: impl Into<String>) {
        self.push_user(&Prompt::text(text));
    }

    /// What was said, with whatever came with it.
    pub fn push_user(&mut self, prompt: &Prompt) {
        // Saying something in a placeholder is what makes it a conversation. Here rather than at
        // any of the call sites because there is more than one way to speak into a session — the
        // composer, a steering message taken in mid-turn, a peer on another machine — and the one
        // that forgot would leave a conversation with messages in it that no list ever shows.
        self.ephemeral = false;
        self.messages.push(Message { role: Role::User, content: prompt.blocks() });
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
    ///
    /// `window` of zero is *not said*, never *gone*. A driver that reports a count without a
    /// denominator — which is what a compaction boundary is — must not take the percentage off a
    /// meter at the exact moment the number finally moved.
    pub fn set_context(&mut self, used: u64, window: u64) {
        self.context_tokens = used;
        self.context_counted = true;
        if window > 0 {
            self.context_window = Some(window);
        }
    }

    pub fn add_usage(&mut self, u: &Usage) {
        // The most recent request's prompt is what fills the window. Replaced rather than summed:
        // adding them would report a conversation as longer than any request it ever made.
        //
        // Skipped entirely once a driver has told us: this is the estimate, and it is wrong in the
        // direction that matters — it counts one request rather than the conversation the agent is
        // holding, which for a CLI that compacts on its own is not the same thing at all.
        let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
        if prompt > 0 && !self.context_counted {
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
            repo_root: None,
            branch: None,
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
            on_screen: false,
            unread: self.unread,
            interrupted: self.interrupted,
            background: self.background.clone(),
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
    fn a_picture_can_carry_a_message_on_its_own() {
        // Several providers reject an empty text block, so a message that is only a screenshot
        // must not have one bolted onto it.
        let p = Prompt {
            text: String::new(),
            images: vec![ContentBlock::Image {
                path: "/x/a.png".into(),
                media_type: "image/png".into(),
            }],
        };
        assert_eq!(p.blocks().len(), 1);

        // And a message that is only words is exactly what it always was, empty or not: dropping
        // the block here would leave nothing, and a driver handed nothing re-asks the question
        // before it.
        assert_eq!(Prompt::text("").blocks(), vec![ContentBlock::Text { text: String::new() }]);
    }

    #[test]
    fn a_question_points_at_its_picture_rather_than_trailing_it() {
        let p = Prompt {
            text: "what is wrong here".into(),
            images: vec![ContentBlock::Image {
                path: "/x/a.png".into(),
                media_type: "image/png".into(),
            }],
        };
        assert!(matches!(p.blocks()[0], ContentBlock::Image { .. }), "the picture comes first");
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
