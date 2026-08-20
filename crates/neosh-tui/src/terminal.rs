//! The terminal frontend: crossterm in, ratatui out.
//!
//! Everything terminal-specific is confined here. The rest of the crate works against
//! [`Mirror`](crate::mirror::Mirror) and [`InputEvent`], which is what makes the snapshot tests
//! possible and what a second frontend would replace.

use std::io::{self, Stdout};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode as CtCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use crossterm::{execute, ExecutableCommand};
use neosh_proto::{InputEvent, KeyCode, KeyMods, KeyPress};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::Terminal;

use crate::mirror::Mirror;
use crate::render;
use crate::theme::{ColorDepth, Theme};

/// Translate a crossterm event.
///
/// Returns `None` for events that carry nothing the protocol models — notably key *release*, which
/// on terminals with the Kitty protocol would otherwise deliver every keystroke twice.
pub fn to_input_event(ev: Event) -> Option<InputEvent> {
    match ev {
        Event::Key(KeyEvent { code, modifiers, kind, .. }) => {
            if kind == KeyEventKind::Release {
                return None;
            }
            let code = match code {
                CtCode::Char(c) => KeyCode::Char { c: c.to_string() },
                CtCode::Enter => KeyCode::Enter,
                CtCode::Tab => KeyCode::Tab,
                CtCode::BackTab => KeyCode::BackTab,
                CtCode::Backspace => KeyCode::Backspace,
                CtCode::Delete => KeyCode::Delete,
                CtCode::Insert => KeyCode::Insert,
                CtCode::Esc => KeyCode::Esc,
                CtCode::Left => KeyCode::Left,
                CtCode::Right => KeyCode::Right,
                CtCode::Up => KeyCode::Up,
                CtCode::Down => KeyCode::Down,
                CtCode::Home => KeyCode::Home,
                CtCode::End => KeyCode::End,
                CtCode::PageUp => KeyCode::PageUp,
                CtCode::PageDown => KeyCode::PageDown,
                CtCode::F(n) => KeyCode::F { n },
                _ => return None,
            };
            // Terminals already fold shift into the character, so reporting it as well would make
            // `<S-a>` and `A` two different bindings for the same keystroke. `BackTab` is the same
            // situation spelled differently: it *is* shift-tab, arriving as its own sequence, and
            // crossterm reports the modifier as well. Keeping both means the key that arrives is
            // shift-back-tab, which is not a key anybody can write down or press.
            let shift = modifiers.contains(KeyModifiers::SHIFT)
                && !matches!(code, KeyCode::Char { .. } | KeyCode::BackTab);
            Some(InputEvent::Key {
                key: KeyPress {
                    code,
                    mods: KeyMods {
                        ctrl: modifiers.contains(KeyModifiers::CONTROL),
                        alt: modifiers.contains(KeyModifiers::ALT),
                        shift,
                        meta: modifiers.contains(KeyModifiers::SUPER),
                    },
                },
            })
        }
        // Bracketed paste arrives whole rather than as a burst of synthetic keystrokes, so a
        // pasted `<Esc>` cannot interrupt the agent.
        Event::Paste(text) => Some(InputEvent::Paste { text }),
        Event::Resize(width, height) => Some(InputEvent::Resize { width, height }),
        _ => None,
    }
}

pub struct TerminalFrontend {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    theme: Theme,
}

impl TerminalFrontend {
    /// Take over the terminal.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal, theme: Theme::new(ColorDepth::detect()) })
    }

    pub fn size(&self) -> io::Result<(u16, u16)> {
        let r = self.terminal.size()?;
        Ok((r.width, r.height))
    }

    /// Draw one frame, and report where every window ended up.
    ///
    /// The geometry goes back to the core as `ViewportChanged`. Without it a plugin cannot learn
    /// how wide its pane is, and therefore cannot size a meter, decide what to drop at 60 columns,
    /// or page by a screenful — all of which are the frontend's knowledge, not the core's.
    pub fn draw(
        &mut self,
        mirror: &Mirror,
    ) -> io::Result<Vec<(neosh_proto::WindowId, neosh_proto::Rect)>> {
        self.theme.sync(&mirror.highlights);
        let theme = self.theme.clone();
        let mut caret = None;
        let mut geometry = Vec::new();
        self.terminal.draw(|f| {
            geometry = render::resolve_layout(mirror, f.area());
            caret = render::draw(f, mirror, &theme);
        })?;
        // Placed after the draw, because ratatui hides the cursor for each frame it paints.
        match caret {
            Some((x, y)) => {
                self.terminal.show_cursor()?;
                self.terminal.set_cursor_position((x, y))?;
            }
            None => self.terminal.hide_cursor()?,
        }
        // Handed back as the protocol's own rectangle, not ratatui's: the binary wires the
        // frontend to the core and has no business depending on a rendering library.
        Ok(geometry
            .into_iter()
            .map(|(win, r): (neosh_proto::WindowId, Rect)| {
                (win, neosh_proto::Rect { row: r.y, col: r.x, width: r.width, height: r.height })
            })
            .collect())
    }

    /// Put text on the system clipboard, via OSC 52.
    ///
    /// The escape rather than a clipboard library, for one reason that decides it: neosh is used
    /// over SSH, and a library talks to the X11 or Wayland display on the machine the process is
    /// running on — which is the wrong machine. OSC 52 travels back through the same stream the UI
    /// is drawn on, so it reaches the terminal the human is actually sitting at.
    ///
    /// Terminals that do not implement it ignore it silently. That is the honest failure here: we
    /// cannot detect support, and the alternative — refusing to copy on the chance it will not
    /// work — is worse than a key that does nothing on an old terminal.
    pub fn copy(&mut self, text: &str) -> io::Result<()> {
        use std::io::Write;
        // Terminals cap what they will accept, and a truncated payload is worse than a refused one:
        // you would paste a fragment and not know. 64 KiB is well inside every implementation.
        const LIMIT: usize = 64 * 1024;
        let payload = if text.len() > LIMIT { &text[..floor_char(text, LIMIT)] } else { text };
        let encoded = base64(payload.as_bytes());
        // Straight to stdout, which is the stream the backend draws on. Going through the backend
        // would need an unstable ratatui accessor for no gain: this is one escape, not a frame.
        let mut out = io::stdout().lock();
        write!(out, "\x1b]52;c;{encoded}\x07")?;
        out.flush()
    }

    /// Hand the terminal back.
    ///
    /// Called explicitly on shutdown *and* from `Drop`, because a panic that skipped this would
    /// leave the user's terminal in raw mode with no echo — effectively broken until they blindly
    /// type `reset`.
    pub fn leave(&mut self) -> io::Result<()> {
        disable_raw_mode()?;
        self.terminal.backend_mut().execute(DisableBracketedPaste)?;
        self.terminal.backend_mut().execute(LeaveAlternateScreen)?;
        self.terminal.show_cursor()?;
        Ok(())
    }
}

/// Put the terminal back, from anywhere — including a panic hook.
///
/// A panic inside the alternate screen is invisible: the message is printed onto a screen that is
/// then torn down, so the user sees their prompt and nothing else. Every error is reported as
/// "it just doesn't start". Restoring *before* the message prints is what makes a crash legible.
pub fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = out.execute(DisableBracketedPaste);
    let _ = out.execute(LeaveAlternateScreen);
    let _ = out.execute(crossterm::cursor::Show);
}

impl Drop for TerminalFrontend {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// The largest index at or below `at` that is a character boundary.
fn floor_char(s: &str, at: usize) -> usize {
    let mut i = at.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Standard base64, which OSC 52 requires.
///
/// Written out rather than pulled in: it is fifteen lines, it is the only thing we would use a
/// dependency for, and a base64 crate in the tree invites someone to reach for it in a place where
/// the right answer is not to encode anything at all.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: CtCode, mods: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent { code, modifiers: mods, kind, state: KeyEventState::NONE })
    }

    #[test]
    fn ctrl_combinations_survive_translation() {
        let ev = to_input_event(key(CtCode::Char('p'), KeyModifiers::CONTROL, KeyEventKind::Press));
        match ev {
            Some(InputEvent::Key { key }) => {
                assert_eq!(key.code, KeyCode::Char { c: "p".into() });
                assert!(key.mods.ctrl);
            }
            other => panic!("expected a key event, got {other:?}"),
        }
    }

    #[test]
    fn key_release_is_dropped_so_keystrokes_do_not_double() {
        assert!(
            to_input_event(key(CtCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Release))
                .is_none()
        );
        assert!(
            to_input_event(key(CtCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat))
                .is_some(),
            "auto-repeat is a real keystroke"
        );
    }

    #[test]
    fn shift_is_not_reported_for_characters_that_already_encode_it() {
        let ev = to_input_event(key(CtCode::Char('A'), KeyModifiers::SHIFT, KeyEventKind::Press));
        match ev {
            Some(InputEvent::Key { key }) => {
                assert_eq!(key.code, KeyCode::Char { c: "A".into() });
                assert!(!key.mods.shift, "otherwise <S-a> and A become different bindings");
            }
            other => panic!("unexpected {other:?}"),
        }
        // For non-character keys, shift is meaningful.
        let ev = to_input_event(key(CtCode::Tab, KeyModifiers::SHIFT, KeyEventKind::Press));
        assert!(matches!(ev, Some(InputEvent::Key { key }) if key.mods.shift));
    }

    /// Regression: crossterm reports shift-tab as `BackTab` *and* sets the shift modifier, so a
    /// binding written `<S-Tab>` — which is one key press, not two things at once — never matched
    /// what arrived. It stayed listed in the key sheet the whole time, which is the worst way for
    /// a binding to be broken.
    #[test]
    fn back_tab_does_not_also_report_the_shift_that_produced_it() {
        let ev = to_input_event(key(CtCode::BackTab, KeyModifiers::SHIFT, KeyEventKind::Press));
        match ev {
            Some(InputEvent::Key { key }) => {
                assert_eq!(key.code, KeyCode::BackTab);
                assert!(!key.mods.shift);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn escape_maps_to_esc_so_it_can_reach_the_interrupt() {
        let ev = to_input_event(key(CtCode::Esc, KeyModifiers::NONE, KeyEventKind::Press));
        assert!(matches!(ev, Some(InputEvent::Key { key }) if key.code == KeyCode::Esc));
    }

    #[test]
    fn paste_arrives_whole_rather_than_as_keystrokes() {
        let ev = to_input_event(Event::Paste("multi\nline\u{1b}text".into()));
        match ev {
            Some(InputEvent::Paste { text }) => assert!(text.contains('\u{1b}')),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn resize_is_forwarded() {
        assert_eq!(
            to_input_event(Event::Resize(120, 40)),
            Some(InputEvent::Resize { width: 120, height: 40 })
        );
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_test_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn multibyte_text_survives_the_round_trip() {
        // The payload is bytes, not characters: encoding UTF-8 as if it were latin-1 would paste
        // mojibake, which is the classic OSC 52 bug.
        let encoded = base64("héllo \u{4e16}\u{754c}".as_bytes());
        assert_eq!(encoded, "aMOpbGxvIOS4lueVjA==");
    }

    #[test]
    fn a_payload_over_the_limit_is_cut_on_a_character_boundary() {
        let long = "\u{4e16}".repeat(40_000); // 120 KB of three-byte characters
        assert!(std::str::from_utf8(&long.as_bytes()[..floor_char(&long, 64 * 1024)]).is_ok());
    }
}
