//! The terminal frontend.
//!
//! Consumes [`UiEvent`](neosh_proto::UiEvent) and produces [`InputEvent`](neosh_proto::InputEvent).
//! It holds no authority over anything: the core owns all state, and this crate owns only the
//! question of how that state looks on a character grid.

pub mod mirror;
pub mod render;
pub mod terminal;
pub mod theme;

pub use mirror::Mirror;
pub use render::{display_col, render_line, resolve_layout, truncate_to_width, width};
pub use terminal::{restore_terminal, TerminalFrontend, to_input_event};
pub use theme::{ColorDepth, Theme};
