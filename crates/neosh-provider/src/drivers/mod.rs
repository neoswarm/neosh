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
//! | [`mock`] | model | none | fixtures, for tests |

pub mod acp;
pub mod anthropic;
pub mod http;
pub mod claude_cli;
pub mod claude_control;
pub mod codex_cli;
pub mod google;
pub mod mock;
pub mod openai;

pub use acp::AcpProvider;
pub use anthropic::AnthropicProvider;
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
}
