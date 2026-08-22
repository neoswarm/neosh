//! Wire types for every neosh boundary.
//!
//! This crate is deliberately logic-free. It exists so that three separate boundaries speak the
//! *same* vocabulary:
//!
//! 1. plugin -> core  ([`ApiCall`] / [`ApiResponse`])
//! 2. core -> plugin  ([`PluginRequest`] / [`PluginEvent`])
//! 3. core -> frontend ([`UiEvent`] / [`InputEvent`])
//!
//! In-process these travel as Rust values over an mpsc channel; out-of-process they travel as JSON
//! over stdio. The shape is identical either way, which is what makes "a plugin can live in another
//! process and another language" true rather than aspirational.
//!
//! Every type here derives [`ts_rs::TS`]. `cargo test -p neosh-proto` regenerates
//! `plugins/api/src/generated/`, and CI fails if the result differs from what is committed.

pub mod agent;
pub mod ascp;
pub mod api;
pub mod ids;
pub mod options;
pub mod plugin;
pub mod provider;
pub mod quota;
pub mod ui;
pub mod vcs;

pub use agent::*;
pub use ascp::*;
pub use api::*;
pub use ids::*;
pub use options::*;
pub use plugin::*;
pub use provider::*;
pub use quota::*;
pub use ui::*;
pub use vcs::*;

/// Protocol version. Bumped on any breaking change to the types in this crate; the host refuses to
/// load a plugin whose `@neosh/api` was built against a different major.
pub const PROTOCOL_VERSION: u32 = 1;
