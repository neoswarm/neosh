//! The UI event protocol: core -> frontend, and frontend -> core.
//!
//! # Why this boundary exists
//!
//! The core never calls ratatui and never assumes a terminal. It emits *retained state deltas*; a
//! frontend folds them into a mirror and draws. That indirection is what lets a web or mobile
//! frontend be a new consumer rather than a rewrite.
//!
//! # Layout is the frontend's job
//!
//! Geometry on this wire is *declarative* ([`WindowLayout`], [`FloatConfig`], [`Extent`]) — a dock
//! and a preferred size, or an anchor and an offset. The frontend resolves that against a real
//! viewport, does all display-width math, and reports the result back via
//! [`InputEvent::ViewportChanged`]. If the core emitted concrete cell rectangles instead, the
//! protocol would have a character grid baked into it and every non-terminal frontend would inherit
//! terminal assumptions.
//!
//! # Text and annotations travel together
//!
//! [`UiEvent::BufferLines`] is the *only* buffer mutation event, and it carries each line's text and
//! its extmarks in the same payload ([`LineRender`]). There is deliberately no way to update text
//! without its marks or marks without their text, which makes the classic "virtual text desyncs
//! from the line it was attached to" bug unrepresentable on the wire.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::ids::{BufferId, ExtmarkId, NamespaceId, SurfaceId, WindowId};

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Where a non-floating window sits.
///
/// Deliberately *not* an arbitrary split tree: agent UIs do not need one, and a split tree creates
/// layout ambiguity that every plugin then has to reason about.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Dock {
    Left,
    Right,
    Bottom,
    /// The primary region. Exactly one window occupies it at a time.
    Main,
}

/// What a float is positioned relative to.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum Anchor {
    /// Follows the cursor. The only anchor requiring viewport feedback from the frontend.
    Cursor,
    /// Positioned within another window's rectangle.
    Window { win: WindowId },
    /// Positioned within the whole screen.
    Screen,
}

/// A size request along one axis. Resolved by the frontend against the real viewport.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum Extent {
    /// Exactly `n` cells.
    Fixed { n: u16 },
    /// Fit the content.
    Auto,
    /// Fit the content, but never exceed `n` cells.
    Max { n: u16 },
    /// Fill the available space.
    Fill,
}

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum BorderStyle {
    #[default]
    None,
    Single,
    Rounded,
    Double,
    Thick,
}

/// A cell offset from the anchor point. May be negative to place a float above or left of its
/// anchor.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct Offset {
    pub row: i32,
    pub col: i32,
}

/// The highest-leverage primitive in the system. Pickers, completion, hover, permission prompts and
/// keybinding hints are all this one type with different fields set.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct FloatConfig {
    pub anchor: Anchor,
    #[serde(default)]
    pub offset: Offset,
    pub width: Extent,
    pub height: Extent,
    /// Higher draws on top. Ties break by open order.
    #[serde(default)]
    pub z: i32,
    #[serde(default)]
    pub border: BorderStyle,
    /// Semantic highlight group for the border, e.g. `"MyPlugin.Border"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border_hl: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Close automatically when focus moves elsewhere. The right default for pickers and hovers.
    #[serde(default)]
    pub close_on_blur: bool,
    /// Whether the float can take focus at all. `false` for passive hints and hover cards.
    #[serde(default)]
    pub focusable: bool,
}

impl Default for FloatConfig {
    fn default() -> Self {
        Self {
            anchor: Anchor::Screen,
            offset: Offset::default(),
            width: Extent::Auto,
            height: Extent::Auto,
            z: 100,
            border: BorderStyle::Rounded,
            border_hl: None,
            title: None,
            close_on_blur: false,
            focusable: true,
        }
    }
}

/// Declarative placement for a window. The frontend turns this into a rectangle.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum WindowLayout {
    Docked {
        dock: Dock,
        /// Preferred extent along the dock's variable axis.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u16>,
    },
    Float {
        config: FloatConfig,
    },
}

/// A resolved rectangle in frontend cell space. Only ever travels frontend -> core (as viewport
/// feedback) or core -> frontend for raw-cell surfaces, which are opaque blits by definition.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct Rect {
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
}

// ---------------------------------------------------------------------------
// Highlights
// ---------------------------------------------------------------------------

/// A color as *declared*. Down-conversion to 256 or 16 colors happens at the terminal boundary, so
/// plugins never think about terminal capability.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum Color {
    Rgb { r: u8, g: u8, b: u8 },
    /// An ANSI 256-color index.
    Indexed { i: u8 },
    /// The terminal's own default foreground/background.
    Default,
}

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct Attrs {
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub reverse: bool,
    #[serde(default)]
    pub dim: bool,
    #[serde(default)]
    pub strikethrough: bool,
}

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct HighlightSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<Color>,
    #[serde(flatten)]
    pub attrs: Attrs,
}

/// Plugins declare semantic names, never colors directly.
///
/// [`HighlightDef::Link`] is what makes a plugin look correct under a theme its author never saw:
/// `MyPlugin.Border -> Float.Border` inherits whatever the active theme decided borders look like.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum HighlightDef {
    /// Inherit from another group. Chains are resolved at the terminal boundary; cycles are broken
    /// and reported rather than hung on.
    Link { to: String },
    Spec { spec: HighlightSpec },
}

// ---------------------------------------------------------------------------
// Extmarks
// ---------------------------------------------------------------------------

/// Where virtual text is drawn relative to the line it is attached to.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum VirtTextPos {
    /// Flush against the right edge of the window, on the same screen line.
    ///
    /// Every list row in a workspace is the same shape: flexible text on the left that truncates,
    /// fixed status on the right that must not. A plugin cannot build that itself — it would have
    /// to measure display width, which is the frontend's job precisely so a plugin cannot get it
    /// wrong. `Eol` is not the same thing: it means "after the text", which is wherever the text
    /// happens to end.
    Right,
    /// After the end of the line. The common case for inline diagnostics and token counts.
    #[default]
    Eol,
    /// Spliced into the line at the mark's column, shifting real text right.
    Inline,
    /// On its own line above.
    Above,
    /// On its own line below.
    Below,
}

/// One run of virtual text with a semantic highlight group.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct VirtChunk {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hl_group: Option<String>,
}

/// What happens to a mark when the text it sits on is replaced by content that has no
/// corresponding line.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum OnDelete {
    /// Survive, clamped to the nearest surviving position.
    #[default]
    Clamp,
    /// Become invalid and stop rendering. Queryable so a plugin can clean up.
    Invalidate,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct ExtmarkOpts {
    /// End column for a ranged highlight, as a UTF-8 byte offset. `None` means a point mark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_col: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hl_group: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub virt_text: Vec<VirtChunk>,
    #[serde(default)]
    pub virt_text_pos: VirtTextPos,
    #[serde(default)]
    pub on_delete: OnDelete,
    /// Higher priority draws over lower when marks overlap.
    #[serde(default)]
    pub priority: i32,
}

/// A mark as it appears on the wire, already resolved to a column on a specific line.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct ExtmarkRender {
    pub ns: NamespaceId,
    pub id: ExtmarkId,
    /// UTF-8 byte offset into the line. The frontend converts to a display column.
    pub col: u32,
    #[serde(flatten)]
    pub opts: ExtmarkOpts,
}

/// A mark's current position, for `get`.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct ExtmarkInfo {
    pub id: ExtmarkId,
    pub ns: NamespaceId,
    pub row: u32,
    pub col: u32,
    pub invalid: bool,
    #[serde(flatten)]
    pub opts: ExtmarkOpts,
}

/// One line of a buffer, with its annotations attached.
///
/// Text and marks are never sent separately. See the module docs.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct LineRender {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub marks: Vec<ExtmarkRender>,
}

// ---------------------------------------------------------------------------
// Raw cells
// ---------------------------------------------------------------------------

/// A directly-addressed cell within a claimed surface.
///
/// The escape hatch for the ~5% of plugins (diff gutters, sparklines, token/cost graphs) that
/// cannot be expressed as text plus annotations. A non-terminal frontend renders a surface as an
/// opaque monospace grid.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct SurfaceCell {
    pub row: u16,
    pub col: u16,
    /// One grapheme cluster, not one `char`. Emoji and combining sequences are single cells here.
    pub grapheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fg: Option<Color>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<Color>,
    #[serde(default)]
    pub attrs: Attrs,
}

// ---------------------------------------------------------------------------
// Core -> frontend
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum MessageLevel {
    Info,
    Warn,
    Error,
}

/// A retained-state delta. Frontends fold these into a mirror and draw from it.
///
/// Emitted in coalesced batches — see [`UiEvent::Flush`].
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum UiEvent {
    /// Always first. A frontend that sees a `protocol_version` it does not implement must refuse to
    /// continue rather than render a partial UI.
    Init {
        protocol_version: u32,
    },

    BufferOpened {
        buf: BufferId,
        name: String,
    },
    BufferClosed {
        buf: BufferId,
    },
    /// The only buffer mutation. Apply as `mirror.splice(start..old_end, lines)` — the exact
    /// operation the core performed, so mirror and core cannot diverge.
    BufferLines {
        buf: BufferId,
        start: u32,
        old_end: u32,
        lines: Vec<LineRender>,
    },

    WindowOpened {
        win: WindowId,
        buf: BufferId,
        layout: WindowLayout,
    },
    WindowConfigured {
        win: WindowId,
        layout: WindowLayout,
    },
    WindowBuffer {
        win: WindowId,
        buf: BufferId,
    },
    WindowClosed {
        win: WindowId,
    },
    CursorMoved {
        win: WindowId,
        row: u32,
        col: u32,
    },
    /// Scroll position requested by the core (e.g. follow streaming output).
    ScrollTo {
        win: WindowId,
        top_line: u32,
    },

    HighlightDefined {
        name: String,
        def: HighlightDef,
    },

    SurfaceClaimed {
        surface: SurfaceId,
        win: WindowId,
        rect: Rect,
    },
    SurfaceCells {
        surface: SurfaceId,
        cells: Vec<SurfaceCell>,
    },
    SurfaceReleased {
        surface: SurfaceId,
    },

    FocusChanged {
        win: Option<WindowId>,
    },

    /// Transient status text. Not a buffer, so it never pollutes conversation history.
    Message {
        level: MessageLevel,
        text: String,
    },

    /// Put text on the system clipboard.
    ///
    /// The frontend does it because only the frontend has a terminal: over a plain pty, the way to
    /// reach the clipboard is the OSC 52 escape, and that has to be written to the same stream the
    /// UI is drawn on. It also means copying works over SSH, where a library talking to X11 or
    /// Wayland would be talking to the wrong machine.
    Clipboard {
        text: String,
    },

    /// End of a coalesced batch — the frontend should draw at most once per `Flush`.
    ///
    /// Batches are emitted on a ~16ms deadline armed by the first mutation, not on a frame timer.
    /// An idle session emits nothing and costs zero CPU; a burst of streaming tokens produces one
    /// `Flush`, not one per token.
    Flush,

    Shutdown,
}

// ---------------------------------------------------------------------------
// Text editing
// ---------------------------------------------------------------------------

/// Where the cursor goes.
///
/// Resolved against the *buffer*, never the screen: the core does not know how wide a pane is or
/// how many columns a character occupies. `Up` and `Down` therefore remember a grapheme-cluster
/// index rather than a display column, which is the closest thing to "the same place" that can be
/// computed without measuring anything.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum CursorMotion {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    /// To the start of the previous word, stepping over punctuation in one go.
    WordLeft,
    WordRight,
    BufStart,
    BufEnd,
}

/// A change to make at a window's cursor.
///
/// Deliberately verbs rather than a text diff: a plugin that wants "delete the word behind the
/// cursor" should not have to reimplement word boundaries, and every caller getting the same answer
/// is the only way `<C-w>` means one thing across the whole program.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum TextEdit {
    /// Text at the cursor. May contain newlines — that is how both a paste and `<S-CR>` work.
    Insert { text: String },
    /// The grapheme before the cursor, joining lines when there is none.
    DeleteBack,
    /// The grapheme after the cursor, joining lines when there is none.
    DeleteForward,
    DeleteWordBack,
    DeleteWordForward,
    DeleteToLineStart,
    DeleteToLineEnd,
    /// Everything between two positions, in either order. Columns are UTF-8 byte offsets.
    DeleteRange { from: (u32, u32), to: (u32, u32) },
    /// Everything between the selection anchor and the cursor. Nothing, if nothing is selected.
    DeleteSelection,
}

// ---------------------------------------------------------------------------
// Frontend -> core
// ---------------------------------------------------------------------------

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum KeyCode {
    /// One grapheme cluster. A string rather than a `char` so IME output and emoji survive.
    Char { c: String },
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Insert,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F { n: u8 },
}

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[ts(export)]
pub struct KeyMods {
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub alt: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub meta: bool,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)]
#[ts(export)]
pub struct KeyPress {
    pub code: KeyCode,
    #[serde(default)]
    pub mods: KeyMods,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum InputEvent {
    /// Sent once on connect, before anything else.
    Ready { width: u16, height: u16 },
    Key { key: KeyPress },
    /// Bracketed paste arrives whole rather than as synthetic keystrokes.
    Paste { text: String },
    Resize { width: u16, height: u16 },
    /// Layout feedback. The core needs this to resolve cursor-anchored floats and to know how far a
    /// `PageDown` should scroll; it is the only thing the core learns about real geometry.
    ViewportChanged {
        win: WindowId,
        width: u16,
        height: u16,
        top_line: u32,
    },
    /// Run a registered command by name.
    ///
    /// A frontend that can only send keys cannot have a menu, a button or a palette — every entry
    /// in one of those is a command, and synthesising the keystroke it happens to be bound to would
    /// break the moment the user rebound it. Names are resolved through the same registry a plugin
    /// uses, so a frontend can offer exactly what `cmd.list()` reports and nothing more.
    Command {
        name: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// The frontend is going away (terminal closed, socket dropped).
    Disconnected,
}
