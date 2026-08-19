//! Model providers.
//!
//! # Shape
//!
//! A [`Provider`] is a *driver*: an implementation that can turn a [`TurnRequest`] into a stream of
//! [`ProviderEvent`]. An [`InstanceConfig`] is a *configured* driver — base URL, credentials, model
//! list. One driver serves many instances, which is why "OpenAI and OpenRouter and Groq and a local
//! llama.cpp, simultaneously" needs no new code: they are four instances of `openai-compat`.
//!
//! # Two levels of driver
//!
//! Most drivers are **model drivers**: they expose a raw completion API, and *our* agent loop runs
//! on top, so our tool registry, our hooks and our permission layer all apply. That is the path the
//! Phase 0 definition of done exercises.
//!
//! [`drivers::claude_cli`] is an **agent driver**: it delegates the whole loop to an external agent
//! that brings its own tools. It exists because it works with zero configuration and no API key —
//! it reuses an existing `claude` login — which makes the editor useful the moment it is installed.
//! Its trade-off is explicit in [`Provider::delegates_agent_loop`], and the host surfaces it rather
//! than pretending the two kinds are interchangeable.

pub mod catalog;
pub mod drivers;
pub mod registry;
pub mod sse;

use std::pin::Pin;

use futures::Stream;
use neosh_proto::{InstanceConfig, ModelInfo, ProviderEvent, TurnRequest};
use tokio_util::sync::CancellationToken;

pub use registry::ProviderRegistry;

/// A stream of normalized events. Errors are a [`ProviderEvent::Error`] variant rather than a
/// `Result`, so a partially-successful turn keeps the text it already produced instead of
/// collapsing to a single error at the outer layer.
pub type ProviderStream = Pin<Box<dyn Stream<Item = ProviderEvent> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("no credentials: {0}")]
    MissingCredentials(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("bad response: {0}")]
    BadResponse(String),
}

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn driver(&self) -> neosh_proto::DriverKind;

    /// Whether this driver runs its own agent loop and brings its own tools.
    ///
    /// When true, the host does not send its tool registry and its `tool.pre` hooks observe rather
    /// than gate, because the decision has already been made in another process.
    fn delegates_agent_loop(&self) -> bool {
        false
    }

    /// Ask the endpoint what it serves.
    ///
    /// Discovery beats a hardcoded list: model lineups change weekly, and every
    /// OpenAI-compatible endpoint answers `GET /v1/models`. The configured
    /// [`InstanceConfig::models`] is used as-is when discovery is unavailable.
    async fn list_models(&self, instance: &InstanceConfig) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(instance.models.clone())
    }

    /// Start a turn. Dropping the returned stream, or cancelling the token, must terminate any
    /// in-flight request and reap any child process.
    fn stream(
        &self,
        instance: &InstanceConfig,
        request: TurnRequest,
        cancel: CancellationToken,
    ) -> ProviderStream;
}

/// Resolve a secret from the environment.
///
/// Secrets are read at request time and never stored in a type that crosses a process boundary, is
/// serialized, or is `Debug`-printed. [`secrecy::SecretString`] makes the last one structural
/// rather than a review checklist item.
pub fn resolve_auth(auth: &neosh_proto::AuthRef) -> Result<Option<secrecy::SecretString>, ProviderError> {
    match auth {
        neosh_proto::AuthRef::None | neosh_proto::AuthRef::Inherited => Ok(None),
        neosh_proto::AuthRef::Env { var } => match std::env::var(var) {
            Ok(v) if !v.trim().is_empty() => Ok(Some(secrecy::SecretString::from(v))),
            _ => Err(ProviderError::MissingCredentials(format!(
                "environment variable {var} is not set"
            ))),
        },
    }
}
