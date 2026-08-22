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

use crate::ids::{BufferId, ExtmarkId, NamespaceId, SessionId, SurfaceId, WindowId};

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
    /// Positioned against the corner of a dock's strip, whatever is docked there.
    ///
    /// The anchor a completion menu needs, and the one it could not otherwise have: the thing it
    /// is completing is the message field, the message field is whatever is docked at the bottom,
    /// and a plugin has no way to name that window. `Anchor::Screen` puts a `/` menu in the middle
    /// of the transcript it is nothing to do with; `Anchor::Cursor` follows a caret that is inside
    /// the field and therefore under the menu.
    ///
    /// The float lands *flush against* that strip on the main region's side of it, so a menu over
    /// the composer is `Anchor::Dock { dock: Dock::Bottom }` and nothing else — no height to
    /// subtract, no offset to keep in step with how many rows the list is showing. An offset from
    /// there still means what it always means: a negative row lifts it further clear.
    ///
    /// `Dock::Main` is the main region's own top-left, for a float that wants to sit over the
    /// transcript and nothing else.
    Dock { dock: Dock },
}

/// A size request along one axis. Resolved by the frontend against the real viewport.
///
/// # An extent measures content, not the box drawn around it
///
/// Every variant here answers the same question — *how much room does what I am putting in it
/// need* — and the border is chrome the frontend adds afterwards. `Auto` could not mean anything
/// else, since the content is all it has to measure; the others follow it so that swapping one for
/// another does not silently resize what you can see.
///
/// The alternative, where `Fixed` names the outer box, means every caller subtracts two from every
/// number it computes, and the day one of them forgets, the last line of a list is simply not
/// drawn — with no error, because a rectangle that small is perfectly valid.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum Extent {
    /// Room for exactly `n` cells of content.
    Fixed { n: u16 },
    /// Fit the content.
    Auto,
    /// Fit the content, but never more than `n` cells of it.
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
        /// Which end content settles against when there is not enough of it to fill the window.
        #[serde(default)]
        gravity: Gravity,
        /// Whether long lines wrap rather than clip. The main dock always wraps; every other dock
        /// clips unless it says otherwise, because a text field is the only chrome whose content
        /// is prose. A bottom dock that wraps also grows to fit what wrapped, `size` becoming its
        /// floor — a field that wraps a long line and then hides the wrapped rows has clipped it
        /// with extra steps.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wrap: Option<bool>,
    },
    Float {
        config: FloatConfig,
    },
}

/// Which end of a window short content settles against.
///
/// A layout property rather than a rendering trick, so it survives the trip through the protocol and
/// a different frontend resolves it the same way. `End` is what makes a transcript read as a
/// conversation: three exchanges should sit just above the field you are typing into, not stranded
/// at the top of an empty screen with the composer a long way below them.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum Gravity {
    #[default]
    Start,
    End,
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

/// Text that moves on its own.
///
/// Declared on a highlight group rather than driven by whoever wrote the text, for two reasons.
/// A plugin animating text itself would have to re-set an extmark per character per tick — forty
/// API calls at 20 Hz, across a runtime boundary, to move a highlight two columns. And motion is a
/// *display* decision: how bright a band can get depends on what the terminal can render, which is
/// the one thing only the frontend knows.
///
/// So the core stores this and forwards it; the frontend animates whatever carries it, at whatever
/// rate it likes, and stops when nothing animated is on screen. Nothing wakes the core.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum Animation {
    /// A band of brightness sweeping along the run, left to right.
    ///
    /// What "it is working" looks like when there is nothing yet to show. Reads as motion at a
    /// glance without asking to be read, which is the whole trick: a spinner you have to look at
    /// costs attention every time it moves.
    Shimmer {
        /// One full sweep, in milliseconds.
        #[ts(type = "number")]
        period_ms: u32,
    },
    /// The whole run brightening and dimming together.
    Pulse {
        #[ts(type = "number")]
        period_ms: u32,
    },
    /// The run is *replaced* by successive glyphs — a spinner.
    ///
    /// The one animation that changes what is drawn rather than what colour it is, and the only
    /// one whose run has to be a single glyph: the frontend clips or pads each frame to the
    /// display width of the text underneath, so a spinner can never shift the column after it
    /// however badly chosen its frames are. The buffer still holds the glyph the writer wrote,
    /// which is what `^S` searches and what a yank copies out — a frame is what a cell *looks*
    /// like for a sixteenth of a second, not what the line says.
    ///
    /// Which glyphs, though, is the frontend's: a frame set travels as a name because the sets
    /// that read well are the ones drawn from a font the terminal actually has, and a plugin
    /// picking codepoints cannot know that. Every set has an ASCII fallback for the terminals
    /// that would otherwise draw a column of replacement characters.
    Frames {
        set: FrameSet,
        /// One full cycle through the set, in milliseconds.
        #[ts(type = "number")]
        period_ms: u32,
    },
    /// One brightening, once, and then never again — what a row does the moment it lands.
    ///
    /// The only animation here that is not a function of the clock alone: "once" needs a moment to
    /// count from, and the moment is the first time the frontend saw the mark carrying it. Which
    /// makes the mark the unit — an extmark is created once, so a flash fires once, and scrolling
    /// the row away and back does not fire it again.
    ///
    /// Meant for [`ExtmarkOpts::line_hl_group`], where it lifts every span on the row together.
    /// On a ranged group it lifts that run alone, which is legible but rarely what you want: a
    /// card that landed is one event, not four coloured pieces that each had an idea.
    Flash {
        /// How long the lift takes to fall back to nothing.
        #[ts(type = "number")]
        ms: u32,
    },
}

/// The glyphs a [`Animation::Frames`] cycles through.
///
/// Named rather than carried, so the frontend can answer the two questions a plugin cannot: what
/// this terminal can draw, and how wide it comes out. Adding one here is adding it for everybody.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum FrameSet {
    /// `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` — the quietest of these at a small size, and the default for that reason.
    #[default]
    Braille,
    /// `⠁⠂⠄⡀⢀⠠⠐⠈` — one dot orbiting. Slower to read as motion, quieter still.
    Dots,
    /// `▁▂▃▄▅▆▇▆▅▄▃▂` — a bar breathing. The loudest, for something that wants finding.
    Bars,
    /// `◐◓◑◒` — a half-disc turning.
    Arc,
    /// `▖▘▝▗` — a corner walking around a cell.
    Corners,
}

impl Animation {
    pub fn period_ms(self) -> u32 {
        match self {
            Self::Shimmer { period_ms } | Self::Pulse { period_ms } | Self::Frames { period_ms, .. } => {
                period_ms.max(50)
            }
            Self::Flash { ms } => ms.max(50),
        }
    }

    /// Whether this animation runs for ever, or fires once and is over.
    ///
    /// What decides whether a frame being drawn is a reason to ask for another one. A flash that
    /// has finished must stop costing frames, or an idle transcript full of landed cards keeps the
    /// ticker alive for ever.
    pub fn repeats(self) -> bool {
        !matches!(self, Self::Flash { .. })
    }
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
    /// Motion. Ignored when `ui.motion` is off, which is why it is a hint on the group rather than
    /// a promise to the plugin that set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animate: Option<Animation>,
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
    /// A background for the whole rendered row, not just the bytes the mark covers.
    ///
    /// The distinction is the difference between a diff line that is green and a diff line whose
    /// *text* is green: the band has to reach the right edge of the window to read as one line, and
    /// how wide the window is is not known where the row is written. So the group is carried and
    /// the frontend fills to its own edge — which is also what makes the band survive a resize.
    ///
    /// It sits *under* every ranged `hl_group` on the row rather than competing with them, so a
    /// syntax colour on top keeps its foreground and inherits this background. Nothing else in the
    /// mark vocabulary composes; this one has to, because "which line changed" and "what does this
    /// word mean" are two facts about the same character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_hl_group: Option<String>,
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
    /// Sent once on connect, before anything else, by a frontend that has been listening since the
    /// core started.
    Ready { width: u16, height: u16 },
    /// Sent once on connect by a frontend that has *not* — one attaching to a workspace that was
    /// already running, and whose mirror is therefore empty.
    ///
    /// Distinct from [`InputEvent::Ready`] because the answer to it is different: everything has
    /// to be said again from the beginning ([`crate::UiEvent`] is a delta stream, and a delta into
    /// nothing is nothing). Folding the two together and always re-announcing would mean the
    /// in-process case sends its whole state twice at startup, into a mirror that already has it.
    Attached { width: u16, height: u16 },
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
    /// Draw the same state again.
    ///
    /// The frontend asks for this while something on screen is animating, and stops asking the
    /// moment nothing is. It carries no state and mutates nothing — the core answers by flushing
    /// what it already had, which is why an animation cannot desynchronise from the buffer it is
    /// drawn over.
    ///
    /// This is the *only* thing that draws a frame nobody asked for, and it exists on the frontend's
    /// terms: there is still no frame loop, and an idle workspace still sends nothing at all.
    Repaint,
    /// The frontend is going away (terminal closed, socket dropped).
    Disconnected,
}

// ---------------------------------------------------------------------------
// Attaching to a workspace that is already running
// ---------------------------------------------------------------------------

/// What a client says to the workspace it has connected to.
///
/// # Why this is a second envelope rather than raw [`InputEvent`]s
///
/// [`InputEvent`] is what a *view* does — a key, a resize, a command. It has nothing to say about
/// the connection carrying it, and it should not: the same events come from a terminal that owns
/// the process, where there is no connection to talk about. Attaching, detaching and being told
/// somebody else took over are facts about the wire, so they live on the wire's own type and the
/// view protocol stays exactly what it was.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum ClientMessage {
    /// The first message on a connection, and the only one allowed to be first.
    ///
    /// The size comes with it because a workspace that has been running headless has no idea how
    /// big the terminal now looking at it is, and drawing one frame at the wrong size before the
    /// first resize arrives is a visible flash of the wrong layout.
    Attach {
        /// Refused rather than negotiated. A client built against a different protocol is almost
        /// always a neosh that was upgraded while its workspace kept running, and rendering a
        /// partial UI is a worse answer than saying so.
        protocol_version: u32,
        width: u16,
        height: u16,
    },
    Input {
        #[serde(flatten)]
        event: InputEvent,
    },
    /// Leave. The workspace, and anything running in it, carries on without a viewer.
    Detach,
    /// Leave, and take the workspace with you.
    Stop,
    /// What is running, for a client that only wants to ask.
    Status,
}

/// What the workspace says back.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum ServerMessage {
    /// The attach was accepted. Everything needed to draw follows immediately.
    Attached { protocol_version: u32 },
    /// It was not, and this is why. The connection closes after this.
    Refused { reason: String, protocol_version: u32 },
    /// One coalesced batch, ending in [`UiEvent::Flush`] — the same batch a frontend in the same
    /// process would have been handed.
    Events { batch: Vec<UiEvent> },
    /// This client is no longer the one attached.
    Detached { reason: DetachReason },
    Status { status: WorkspaceStatus },
}

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum DetachReason {
    /// The client asked to leave.
    Asked,
    /// Another client attached. One view at a time, and the newest one wins — a client that has
    /// gone away without saying so is indistinguishable from one that is merely quiet, so refusing
    /// the new terminal would mean a crashed one locks you out of your own workspace until it is
    /// noticed and killed.
    TakenOver,
    /// The workspace is shutting down.
    Stopping,
}

/// What a workspace is holding, for `neosh status`.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[ts(export)]
pub struct WorkspaceStatus {
    /// Seconds since the workspace started.
    #[ts(type = "number")]
    pub uptime_secs: u64,
    /// Seconds since a client was last attached, or 0 while one is.
    #[ts(type = "number")]
    pub idle_secs: u64,
    pub attached: bool,
    pub conversations: usize,
    /// One line per conversation with a turn in flight: what it is called and what it is doing.
    pub running: Vec<RunningTurn>,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct RunningTurn {
    pub session: SessionId,
    pub label: String,
    /// The directory the conversation belongs to.
    pub cwd: String,
    #[ts(type = "number")]
    pub elapsed_secs: u64,
}
