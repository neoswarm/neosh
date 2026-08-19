//! Windows: viewports onto buffers.
//!
//! A window carries *declarative* geometry only. It never knows its pixel or cell rectangle — the
//! frontend resolves that and reports back via [`Window::viewport`], which the core needs solely to
//! place cursor-anchored floats and to size a page scroll.

use neosh_proto::{BufferId, WindowId, WindowLayout};

/// What the frontend told us about a window's realized geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: u16,
    pub height: u16,
    pub top_line: u32,
}

#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub buf: BufferId,
    pub layout: WindowLayout,
    /// `(row, byte_col)` in the buffer.
    pub cursor: (u32, u32),
    /// Where a selection started, if one is running. The other end is always the cursor.
    ///
    /// Held per window rather than per buffer: the same buffer shown twice is two places you can
    /// be, and a selection belongs to the place, not to the text.
    pub anchor: Option<(u32, u32)>,
    /// The column vertical motion is trying to get back to, as a grapheme-cluster index.
    ///
    /// Cleared by anything horizontal. Without it, moving down past a short line and back up
    /// leaves you at the short line's width — the cursor loses its place and never finds it again.
    pub goal_col: Option<u32>,
    /// `None` until the frontend has laid this window out at least once.
    pub viewport: Option<Viewport>,
}

impl Window {
    pub fn new(id: WindowId, buf: BufferId, layout: WindowLayout) -> Self {
        Self { id, buf, layout, cursor: (0, 0), anchor: None, goal_col: None, viewport: None }
    }

    pub fn is_float(&self) -> bool {
        matches!(self.layout, WindowLayout::Float { .. })
    }

    /// Whether this window can take focus. Non-focusable floats (hints, hover cards) are skipped by
    /// focus traversal and never intercept keys.
    pub fn focusable(&self) -> bool {
        match &self.layout {
            WindowLayout::Float { config } => config.focusable,
            WindowLayout::Docked { .. } => true,
        }
    }

    pub fn close_on_blur(&self) -> bool {
        match &self.layout {
            WindowLayout::Float { config } => config.close_on_blur,
            WindowLayout::Docked { .. } => false,
        }
    }
}
