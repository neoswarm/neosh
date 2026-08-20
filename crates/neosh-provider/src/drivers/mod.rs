//! Concrete drivers.
//!
//! | driver | kind | credentials | covers |
//! |---|---|---|---|
//! | [`anthropic`] | model | `ANTHROPIC_API_KEY` | Claude, first-party API |
//! | [`openai`] | model | per-instance | OpenAI, OpenRouter, Groq, DeepSeek, xAI, Mistral, Together, Fireworks, Cerebras, Ollama, llama.cpp, LM Studio, vLLM |
//! | [`google`] | model | `GEMINI_API_KEY` | Gemini |
//! | [`claude_cli`] | agent | existing `claude` login | Claude, no API key |
//! | [`codex_cli`] | agent | existing `codex` login | OpenAI, no API key |
//! | [`mock`] | model | none | fixtures, for tests |

pub mod anthropic;
pub mod http;
pub mod claude_cli;
pub mod codex_cli;
pub mod google;
pub mod mock;
pub mod openai;

pub use anthropic::AnthropicProvider;
pub use claude_cli::ClaudeCliProvider;
pub use codex_cli::CodexCliProvider;
pub use google::GoogleProvider;
pub use mock::MockProvider;
pub use openai::OpenAiCompatProvider;
