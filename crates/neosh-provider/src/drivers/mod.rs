//! Concrete drivers.
//!
//! | driver | kind | credentials | covers |
//! |---|---|---|---|
//! | [`anthropic`] | model | `ANTHROPIC_API_KEY` | Claude, first-party API |
//! | [`openai`] | model | per-instance | OpenAI, OpenRouter, Groq, DeepSeek, xAI, Mistral, Together, Fireworks, Cerebras, Ollama, llama.cpp, LM Studio, vLLM |
//! | [`google`] | model | `GEMINI_API_KEY` | Gemini |
//! | [`claude_cli`] | agent | existing `claude` login | Claude, no API key |
//! | [`codex_cli`] | agent | existing `codex` login | OpenAI, no API key |
//! | [`acp`] | agent | the agent's own login | Cursor, Grok, Gemini — anything speaking ACP |
//! | [`antigravity`] | agent | existing `agy` login | Google Antigravity, no API key |
//! | [`mock`] | model | none | fixtures, for tests |

pub mod acp;
pub mod anthropic;
pub mod antigravity;
pub mod http;
pub mod claude_cli;
pub mod claude_control;
pub mod codex_cli;
pub mod google;
pub mod mock;
pub mod openai;

pub use acp::AcpProvider;
pub use anthropic::AnthropicProvider;
pub use antigravity::AntigravityProvider;
pub use claude_cli::ClaudeCliProvider;
pub use codex_cli::CodexCliProvider;
pub use google::GoogleProvider;
pub use mock::MockProvider;
pub use openai::OpenAiCompatProvider;

/// A driver that runs its own agent loop, and therefore enforces its own permissions.
///
/// The distinction matters because neosh's [`crate::Provider`] trait describes something that
/// streams a model's answer, and these do more than that: they read files, run commands and edit
/// the workspace behind their own policy. neosh's permission mode has to reach that policy, or
/// "full access" is a label on a setting that changes nothing — which is exactly what it was.
///
/// Deliberately not part of `Provider`. Most providers have no such policy to configure, and a
/// method they would all have to ignore is a method that says the wrong thing about what they are.
pub trait AgentDriver: Send + Sync {
    /// Tell the driver what neosh's permission mode is now.
    ///
    /// Applies to a turn that is *already running* wherever the protocol allows it, which is the
    /// only reading of this that matches why anybody presses the key: you tighten or loosen
    /// permissions because of what the agent is doing right now, and a setting that waits for the
    /// next turn arrives after you have already stopped it. A driver that cannot say so mid-turn
    /// applies it to the next one instead — see [`claude_cli`] for which of the four modes are
    /// which, and why.
    fn set_permission_mode(&self, mode: neosh_proto::PermissionMode);

    /// Tell the driver which model a conversation should be using now.
    ///
    /// Per conversation, unlike the mode: several run at once and they need not agree. Defaulted
    /// to doing nothing, because a driver that takes the model as an argument to each turn already
    /// hears about this by being asked the next question.
    fn set_model(
        &self,
        _conversation: &neosh_proto::SessionId,
        _selection: &neosh_proto::ModelSelection,
    ) {
    }

    /// A conversation is gone, and anything being held for it should be let go.
    ///
    /// Defaulted to doing nothing. A driver that holds a process per conversation has one to shut
    /// down; one that does not has nothing to do and should not have to say so.
    fn close_conversation(&self, _conversation: &neosh_proto::SessionId) {}

    /// Give the driver somewhere to send a permission request the mode cannot answer.
    ///
    /// Defaulted to doing nothing, because only a driver that *asks* has anywhere to put this, and
    /// which those are is a fact about the protocol rather than a gap to be filled later. An ACP
    /// agent asks its client over the connection it is already on; `claude -p` and `codex exec`
    /// have no such channel, so for them the mode is the whole conversation about permissions.
    fn set_permission_asker(&self, _asker: std::sync::Arc<dyn crate::approval::PermissionAsker>) {}

    /// Give the driver somewhere to send a *question* — see [`crate::ask`].
    ///
    /// Separate from the asker above, and not because the plumbing is different. A permission
    /// request is answerable by policy and this is not: a driver that pointed both at one place
    /// would be handing "which database should I use?" to a layer whose job is deciding whether to
    /// prompt about writing a file, and whose configured answer is usually "don't".
    ///
    /// Defaulted to doing nothing, for the same reason: a driver with no way to ask has nothing to
    /// put here.
    fn set_question_asker(&self, _asker: std::sync::Arc<dyn crate::ask::QuestionAsker>) {}

    /// Give the driver somewhere to say that its agent has started talking unprompted.
    ///
    /// Defaulted to doing nothing, because most agents only ever answer. See [`Unasked`].
    fn set_unasked(&self, _sink: std::sync::Arc<dyn Unasked>) {}

    /// The next turn in this conversation has nothing to say and is only there to listen.
    ///
    /// The other half of [`Unasked`]: the workspace opens a turn so there is somewhere to *put*
    /// what the agent is already saying, and a turn that spoke would be asking a second question
    /// on top of the one being answered. Consumed by the turn it describes, so it is set
    /// immediately before one is started and never sticks to the one after.
    ///
    /// Defaulted to doing nothing: a driver that never reports an unasked turn is never sent one.
    fn listen_only(&self, _conversation: &neosh_proto::SessionId) {}
}

/// Somewhere to say that an agent has started talking with nobody having asked it to.
///
/// `claude` does this and it is not an edge case: a backgrounded command finishing is enqueued as
/// a message to itself and answered — a whole turn of thinking, tools and text — and the agent
/// saying it will report back when the build lands is the most ordinary thing in the world.
///
/// It belongs to the conversation, so it has to go *in* the conversation, and the only way it can
/// be shown while it is happening is for the workspace to open a turn to hold it. Read only when
/// nothing is attached: a turn that is already running is reading that pipe itself.
pub trait Unasked: Send + Sync + std::fmt::Debug {
    /// The agent in this conversation has begun saying something nobody asked for.
    fn began(&self, conversation: &neosh_proto::SessionId);

    /// Everything running unattended in this conversation, whole, said while no turn was reading.
    ///
    /// The same level [`neosh_proto::Activity::Background`] carries, and here for the reason that
    /// one is a level: the set changes *between* turns — that is what a backgrounded shell is —
    /// and a level nobody is told about is a mark on a row that nothing will ever put right.
    /// [`Self::began`] is not a substitute, because it only fires when the agent goes on to *say*
    /// something about it: the CLI usually does, and a conversation whose indicator depends on
    /// whether it felt like talking is one that is wrong whenever it did not.
    ///
    /// Only when nothing is reading. A turn that is running forwards this itself, in order, on its
    /// own stream — and two paths into one field is how `[a]` lands after `[a, b]`.
    fn background(
        &self,
        conversation: &neosh_proto::SessionId,
        tasks: Vec<neosh_proto::BackgroundTask>,
    );
}
