//! Core state and primitives.
//!
//! This crate owns everything the UI is made of — buffers, windows, floats, extmarks, highlight
//! groups, focus and keymaps — and knows nothing about terminals, async runtimes, or language
//! runtimes. It is synchronous and dependency-light on purpose: every invariant in here is
//! testable without spinning up a runtime, and the same state machine can be driven by a terminal,
//! a test harness, or a JSON-RPC frontend.

pub mod buffer;
pub mod editor;
pub mod focus;
pub mod highlight;
pub mod keymap;
pub mod options;
pub mod palette;
pub mod text;
pub mod window;

pub use buffer::{Buffer, Line, LineEdit, Mark};
pub use editor::{CoreEffect, Editor};
pub use focus::FocusStack;
pub use highlight::HighlightRegistry;
pub use options::OptionRegistry;
pub use keymap::{Binding, KeyResolution, KeySeq, KeymapTable, format_keys, parse_keys};
pub use window::{Viewport, Window};
