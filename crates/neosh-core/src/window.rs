//! Windows: viewports onto buffers.
//!
//! A window carries *declarative* geometry only. It never knows its pixel or cell rectangle — the
//! frontend resolves that and reports back via [`Window::viewport`], which the core needs solely to
//! place cursor-anchored floats and to size a page scroll.

use neosh_proto::{BufferId, CursorShape, SelectShape, WindowId, WindowLayout};

/// What the frontend told us about a window's realized geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: u16,
    pub height: u16,
    pub top_line: u32,
    /// Buffer rows on the screen. Not `height` — one wrapped row is several of those.
    pub rows: u32,
}

#[derive(Debug, Clone)]
pub struct Window {
    pub id: WindowId,
    pub buf: BufferId,
    pub layout: WindowLayout,
    /// `(row, byte_col)` in the buffer.
    pub cursor: (u32, u32),
    /// The first buffer row the window was last asked to show, or `None` for unscrolled.
    ///
    /// What was *asked for*, not what the frontend reported drawing — the two differ for exactly
    /// as long as it takes a frame to come back, and the frontend's report is the one that arrives
    /// too late to be scrolled from. Kept here rather than only in whoever did the scrolling
    /// because a client attaching to a workspace that is already running has to be told where the
    /// transcript sits, and "wherever the top is" would drop it back to the beginning.
    ///
    /// `None` is unscrolled and is the frontend's decision — the end of a transcript, the start of
    /// everything else — which is why it is an `Option` rather than a row that happens to be zero.
    /// Row zero is a place you can scroll to, and it stopped being reachable the moment it shared
    /// an encoding with "wherever this window starts".
    pub top_line: Option<u32>,
    /// Where a selection started, if one is running. The other end is always the cursor.
    ///
    /// Held per window rather than per buffer: the same buffer shown twice is two places you can
    /// be, and a selection belongs to the place, not to the text.
    pub anchor: Option<(u32, u32)>,
    /// What the two ends of the selection mean: whether the character the cursor is on is in it,
    /// and whether the ends snap to whole rows.
    ///
    /// State rather than an invariant the caller re-establishes after every motion. Linewise used
    /// to be the latter — three API calls to move an anchor that is only settable by standing on
    /// it, run again after every key — and every path that moved a cursor without knowing to run
    /// it left a selection of the wrong shape on screen for one frame.
    pub select_shape: SelectShape,
    /// What the caret looks like here. See [`neosh_proto::ApiCall::WinCursorShape`].
    pub cursor_shape: CursorShape,
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
        Self {
            id,
            buf,
            layout,
            cursor: (0, 0),
            top_line: None,
            anchor: None,
            select_shape: SelectShape::default(),
            cursor_shape: CursorShape::default(),
            goal_col: None,
            viewport: None,
        }
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

    /// Whether this window takes the keyboard to itself. See [`FloatConfig::modal`].
    pub fn modal(&self) -> bool {
        match &self.layout {
            WindowLayout::Float { config } => config.modal,
            WindowLayout::Docked { .. } => false,
        }
    }
}
