//! The terminal frontend.
//!
//! Consumes [`UiEvent`](neosh_proto::UiEvent) and produces [`InputEvent`](neosh_proto::InputEvent).
//! It holds no authority over anything: the core owns all state, and this crate owns only the
//! question of how that state looks on a character grid.

pub mod shimmer;
pub mod mirror;
pub mod render;
pub mod terminal;
pub mod theme;

/// Where one window ended up on the screen, and what it actually showed.
///
/// `top_line` is what was *drawn*, which is not always what the core asked for: a window whose
/// scroll had to bend to keep its cursor on screen is showing a different first row, and a plugin
/// paging by a screenful has to page from the one on the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub win: neosh_proto::WindowId,
    pub rect: neosh_proto::Rect,
    pub top_line: u32,
    /// Buffer rows drawn. On a wrapping window this is fewer than the rectangle is tall.
    pub rows: u32,
}

pub use mirror::Mirror;
pub use render::{display_col, render_line, resolve_layout, truncate_to_width, width};
pub use terminal::{restore_terminal, TerminalFrontend, to_input_event};
pub use theme::{ColorDepth, Theme};
