//! A shell in a pane.
//!
//! One `Term` is one pty, one child process and one VT parser. Everything about how a terminal
//! *behaves* lives here; how it is placed and drawn is the pane's business, and the two meet at
//! [`Term::cells`], which hands back the screen as [`SurfaceCell`]s — the raw-cell API a plugin
//! already has.
//!
//! # Why a surface rather than a buffer
//!
//! A buffer is lines of text with marks on them, and a terminal is a grid of cells each with its
//! own colour. Those are not the same thing and the difference is not cosmetic: `vim` inside a pane
//! repaints arbitrary cells in arbitrary colours, and expressing that as extmarks over text would
//! mean rebuilding a mark table every frame at sixty frames a second. The surface API exists for
//! exactly this and carries what a cell needs — grapheme, foreground, background, attributes.
//!
//! # Why a thread
//!
//! A pty read blocks, and there is no portable async pty. One thread per terminal reads until the
//! child goes, feeds the parser, and pokes a channel the host's loop is selecting on. The parser is
//! behind a mutex because two things read it: that thread writing, and the draw taking a snapshot.
//! The lock is never held across an await and never across a draw — [`Term::cells`] copies out and
//! lets go.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use neosh_proto::{Attrs, Color, KeyCode, KeyPress, PaneId, SurfaceCell};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

/// How much scrollback a pane keeps.
///
/// Generous, because the reason to run a build in a pane is to read what it said, and cheap: a cell
/// is a few bytes and this is per terminal rather than per workspace.
const SCROLLBACK: usize = 10_000;

/// A pty, the process on the end of it, and what it has drawn.
pub struct Term {
    /// Kept because resizing goes through it, and because dropping it is what closes the pty.
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Set by the reader thread when the screen has changed, cleared by the draw.
    ///
    /// A flag rather than a queue of damage rectangles: a terminal that has drawn anything at all
    /// is redrawn whole, which for a few thousand cells is cheaper than tracking what moved.
    dirty: Arc<AtomicBool>,
    /// Whether the child has gone. Read on the draw, so a dead shell says so rather than sitting
    /// there looking live.
    done: Arc<AtomicBool>,
    /// What was spawned, for the tab's name.
    pub command: String,
    size: (u16, u16),
}

impl Term {
    /// Start a shell in `cwd`, sized to the pane it is going into.
    ///
    /// `$SHELL` and nothing cleverer. A workspace that guessed — bash because it is always there,
    /// or the login shell read out of `/etc/passwd` — would be a terminal that is not the terminal
    /// the person has configured, and every alias and prompt they rely on would be missing with no
    /// hint as to why.
    pub fn spawn(cwd: &std::path::Path, rows: u16, cols: u16) -> std::io::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        Self::spawn_shell(&shell, cwd, rows, cols)
    }

    /// As [`Term::spawn`], for a named shell.
    ///
    /// Injectable because `$SHELL` is a person's own, with their own configuration in it — which is
    /// exactly right in the product and useless in a test: the first run of this against a real
    /// login shell was answered by an `oh-my-zsh` update prompt that ate a keystroke. A test that
    /// depends on whose machine it runs on is a test that will fail for somebody else.
    pub fn spawn_shell(
        shell: &str,
        cwd: &std::path::Path,
        rows: u16,
        cols: u16,
    ) -> std::io::Result<Self> {
        let size = PtySize { rows: rows.max(1), cols: cols.max(1), pixel_width: 0, pixel_height: 0 };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| std::io::Error::other(format!("openpty: {e}")))?;

        let shell = shell.to_string();
        let mut cmd = CommandBuilder::new(&shell);
        // A login shell, so the profile that defines the prompt and the aliases is read. `-l` is
        // what every terminal emulator passes and what people's dotfiles expect.
        cmd.arg("-l");
        cmd.cwd(cwd);
        // Said out loud so a program in here can tell. Anything that shells out and wants to know
        // whether it is inside the workspace has one thing to test rather than a heuristic.
        cmd.env("NEOSH", "1");
        // A terminal that claims more than the frontend can draw is a terminal drawing escape
        // sequences as text. `xterm-256color` is what the surface can carry: grapheme, two colours
        // and attributes, which is exactly `SurfaceCell`.
        cmd.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| std::io::Error::other(format!("spawn {shell}: {e}")))?;
        // The slave is closed here on purpose: while this process holds it open, the pty never
        // reports EOF and the reader thread below would outlive the shell it is reading.
        drop(pair.slave);

        let writer =
            pair.master.take_writer().map_err(|e| std::io::Error::other(format!("pty: {e}")))?;
        let mut reader =
            pair.master.try_clone_reader().map_err(|e| std::io::Error::other(format!("pty: {e}")))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows.max(1), cols.max(1), SCROLLBACK)));
        let dirty = Arc::new(AtomicBool::new(true));
        let done = Arc::new(AtomicBool::new(false));

        {
            let (parser, dirty, done) = (parser.clone(), dirty.clone(), done.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        // EOF: the child has gone and the pty is closed.
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut p) = parser.lock() {
                                p.process(&buf[..n]);
                            }
                            dirty.store(true, Ordering::Release);
                        }
                    }
                }
                done.store(true, Ordering::Release);
                dirty.store(true, Ordering::Release);
            });
        }

        Ok(Self {
            master: pair.master,
            writer,
            child,
            parser,
            dirty,
            done,
            command: shell,
            size: (rows, cols),
        })
    }

    /// Whether the screen has changed since the last draw. Clears the flag.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// Whether the shell has gone.
    ///
    /// Two signals, because neither is reliable alone. The reader thread sees the pty close, which
    /// is immediate — and on some platforms a master whose child has exited keeps blocking rather
    /// than returning EOF, so a pane would sit there looking live over a dead shell. The child's own
    /// exit status is authoritative and is what settles it; asking costs a `waitpid` with `WNOHANG`
    /// once per draw.
    pub fn finished(&mut self) -> bool {
        if self.done.load(Ordering::Acquire) {
            return true;
        }
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            self.done.store(true, Ordering::Release);
            // So the pane is redrawn once more and can say so.
            self.dirty.store(true, Ordering::Release);
            return true;
        }
        false
    }

    /// Send bytes to the child.
    pub fn write(&mut self, bytes: &[u8]) {
        // A write that fails is a child that has gone, which the reader thread is about to report.
        // Nothing to say here that would not be said twice.
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    /// Tell the child how big it is now.
    ///
    /// Both halves, and they are different mechanisms: the pty carries the `SIGWINCH` the program
    /// reacts to, and the parser is what this end draws from. Resizing one without the other is a
    /// shell that wraps at the old width inside a pane that is a different one.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(rows, cols);
        }
        self.dirty.store(true, Ordering::Release);
    }

    /// Where the child's cursor is, as `(row, col)`, or nothing while it is hidden.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        let p = self.parser.lock().ok()?;
        let screen = p.screen();
        (!screen.hide_cursor()).then(|| screen.cursor_position())
    }

    /// The whole screen, as cells the frontend can paint.
    ///
    /// Every cell every time rather than a diff. The mirror merges by position, so a sparse update
    /// would leave whatever the previous program had drawn in the cells this one has cleared —
    /// which is how you get the last frame of `htop` showing through a shell prompt.
    pub fn cells(&self) -> Vec<SurfaceCell> {
        let Ok(p) = self.parser.lock() else { return Vec::new() };
        let screen = p.screen();
        let (rows, cols) = screen.size();
        let mut out = Vec::with_capacity(rows as usize * cols as usize);
        for row in 0..rows {
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else { continue };
                // A cell the previous character spilled into. Emitting it as a space would paint
                // over the right-hand half of every wide glyph on the screen.
                if !cell.has_contents() && cell.is_wide_continuation() {
                    continue;
                }
                let contents = cell.contents();
                out.push(SurfaceCell {
                    row,
                    col,
                    grapheme: if contents.is_empty() { " ".into() } else { contents.to_string() },
                    fg: convert(cell.fgcolor()),
                    bg: convert(cell.bgcolor()),
                    attrs: Attrs {
                        bold: cell.bold(),
                        italic: cell.italic(),
                        underline: cell.underline(),
                        reverse: cell.inverse(),
                        dim: false,
                        strikethrough: false,
                    },
                });
            }
        }
        out
    }

    /// Ask the child to stop, then make sure it has.
    ///
    /// Closing a pane is not a request the shell may decline. `kill` first so that a program
    /// ignoring `SIGHUP` still goes, and the master dropped after, because dropping it while the
    /// child is alive leaves a process reading from a pty nothing will ever write to.
    pub fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        self.close();
    }
}

/// A `vt100` colour as one the protocol carries.
fn convert(c: vt100::Color) -> Option<Color> {
    match c {
        // "Whatever the terminal already is", which is what the palette calls `Default` and is the
        // one answer that stays right when somebody changes their terminal's theme.
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed { i }),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb { r, g, b }),
    }
}

/// Turn a key press into what a terminal sends for it.
///
/// The part of a terminal emulator that is all detail and no cleverness. What matters is that the
/// set is *complete for the keys neosh can receive* — a key encoded wrongly is not a missing
/// feature, it is a shell that inserts garbage when you press an arrow.
pub fn encode(key: &KeyPress) -> Vec<u8> {
    let ctrl = key.mods.ctrl;
    let alt = key.mods.alt;
    let mut out: Vec<u8> = Vec::new();
    // Alt is the escape prefix, which is what every terminal has meant by it since the VT100.
    if alt {
        out.push(0x1b);
    }
    match &key.code {
        KeyCode::Char { c } => {
            if ctrl {
                // Ctrl clears the top three bits: `^A` is 1, `^Z` is 26. `@[\]^_` and `?` are the
                // rest of the control range and are how `^@` (NUL) and `^?` (DEL) are sent.
                let ch = c.chars().next().unwrap_or(' ');
                let byte = match ch {
                    ' ' | '@' => 0,
                    '?' => 0x7f,
                    c if c.is_ascii() => (c.to_ascii_uppercase() as u8) & 0x1f,
                    _ => {
                        out.extend_from_slice(c.as_bytes());
                        return out;
                    }
                };
                out.push(byte);
            } else {
                out.extend_from_slice(c.as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        // `\x7f`, not `\x08`. A shell reads DEL as backspace and `^H` as a different key, and
        // getting this the wrong way round is the classic "backspace prints ^H".
        KeyCode::Backspace => out.push(0x7f),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Left => out.extend_from_slice(arrow(b'D', ctrl)),
        KeyCode::Right => out.extend_from_slice(arrow(b'C', ctrl)),
        KeyCode::Up => out.extend_from_slice(arrow(b'A', ctrl)),
        KeyCode::Down => out.extend_from_slice(arrow(b'B', ctrl)),
        KeyCode::Home => out.extend_from_slice(b"\x1b[H"),
        KeyCode::End => out.extend_from_slice(b"\x1b[F"),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::F { n } => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
        }
    }
    out
}

/// An arrow, plain or with the modifier form every terminal emits for Ctrl.
fn arrow(letter: u8, ctrl: bool) -> &'static [u8] {
    match (letter, ctrl) {
        (b'A', false) => b"\x1b[A",
        (b'B', false) => b"\x1b[B",
        (b'C', false) => b"\x1b[C",
        (b'D', false) => b"\x1b[D",
        (b'A', true) => b"\x1b[1;5A",
        (b'B', true) => b"\x1b[1;5B",
        (b'C', true) => b"\x1b[1;5C",
        (b'D', true) => b"\x1b[1;5D",
        _ => b"",
    }
}

/// Every terminal in the workspace, by the pane it is in.
#[derive(Default)]
pub struct Terminals {
    live: std::collections::HashMap<PaneId, Term>,
}

impl Terminals {
    pub fn get(&self, pane: PaneId) -> Option<&Term> {
        self.live.get(&pane)
    }

    pub fn get_mut(&mut self, pane: PaneId) -> Option<&mut Term> {
        self.live.get_mut(&pane)
    }

    pub fn insert(&mut self, pane: PaneId, term: Term) {
        self.live.insert(pane, term);
    }

    /// Close a terminal and take it away. The child is killed by `Term`'s `Drop`.
    pub fn remove(&mut self, pane: PaneId) {
        self.live.remove(&pane);
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::KeyMods;

    fn key(code: KeyCode, mods: KeyMods) -> KeyPress {
        KeyPress { code, mods }
    }

    fn ch(c: &str) -> KeyPress {
        key(KeyCode::Char { c: c.into() }, KeyMods::default())
    }

    #[test]
    fn a_letter_is_itself() {
        assert_eq!(encode(&ch("a")), b"a");
        assert_eq!(encode(&ch("Z")), b"Z");
    }

    /// Multi-byte characters go through whole. A terminal that sent one byte of a `é` would corrupt
    /// every command containing one.
    #[test]
    fn a_multibyte_character_survives() {
        assert_eq!(encode(&ch("é")), "é".as_bytes());
        assert_eq!(encode(&ch("🙂")), "🙂".as_bytes());
    }

    #[test]
    fn ctrl_clears_the_top_bits() {
        let ctrl = KeyMods { ctrl: true, ..Default::default() };
        assert_eq!(encode(&key(KeyCode::Char { c: "a".into() }, ctrl)), vec![1]);
        assert_eq!(encode(&key(KeyCode::Char { c: "c".into() }, ctrl)), vec![3]);
        // `^Z`, which is what suspends a job — and is 26 rather than anything to do with 'z'.
        assert_eq!(encode(&key(KeyCode::Char { c: "z".into() }, ctrl)), vec![26]);
        // Case does not matter: `^A` and `^a` are the same key.
        assert_eq!(encode(&key(KeyCode::Char { c: "A".into() }, ctrl)), vec![1]);
    }

    /// The classic: a shell reads DEL as backspace and `^H` as something else entirely.
    #[test]
    fn backspace_is_del_not_backspace() {
        assert_eq!(encode(&key(KeyCode::Backspace, KeyMods::default())), vec![0x7f]);
    }

    #[test]
    fn enter_is_a_carriage_return() {
        // Not `\n`. A pty in canonical mode reads CR as "the line is done"; LF is a line the shell
        // has not been told to run.
        assert_eq!(encode(&key(KeyCode::Enter, KeyMods::default())), b"\r");
    }

    #[test]
    fn arrows_carry_their_modifier() {
        assert_eq!(encode(&key(KeyCode::Up, KeyMods::default())), b"\x1b[A");
        let ctrl = KeyMods { ctrl: true, ..Default::default() };
        assert_eq!(encode(&key(KeyCode::Right, ctrl)), b"\x1b[1;5C");
    }

    #[test]
    fn alt_is_an_escape_prefix() {
        let alt = KeyMods { alt: true, ..Default::default() };
        assert_eq!(encode(&key(KeyCode::Char { c: "b".into() }, alt)), b"\x1bb");
    }

    #[test]
    fn a_default_colour_stays_the_terminals_own() {
        assert_eq!(convert(vt100::Color::Default), None);
        assert_eq!(convert(vt100::Color::Idx(3)), Some(Color::Indexed { i: 3 }));
        assert_eq!(convert(vt100::Color::Rgb(1, 2, 3)), Some(Color::Rgb { r: 1, g: 2, b: 3 }));
    }

    /// The whole round trip, against a real shell: spawn, run something, read it back.
    #[test]
    fn a_shell_runs_a_command_and_the_screen_shows_it() {
        let dir = std::env::temp_dir();
        let Ok(mut t) = Term::spawn_shell("/bin/sh", &dir, 24, 80) else {
            // No pty available — a sandboxed CI container. Nothing to assert, and failing here
            // would be reporting the environment rather than the code.
            return;
        };
        t.write(b"echo neosh-term-works\r");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = false;
        while std::time::Instant::now() < deadline && !seen {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let text: String = t.cells().iter().map(|c| c.grapheme.as_str()).collect();
            // Twice: once as the echoed command line, once as its output. One occurrence is the
            // shell showing what you typed, which proves nothing about it having run.
            seen = text.matches("neosh-term-works").count() >= 2;
        }
        assert!(seen, "the command ran and its output reached the screen");
        t.close();
    }

    #[test]
    fn a_terminal_reports_when_its_shell_has_gone() {
        let Ok(mut t) = Term::spawn_shell("/bin/sh", &std::env::temp_dir(), 24, 80) else { return };
        assert!(!t.finished(), "it starts alive");
        // Wait for it to have drawn something before typing at it. An interactive shell flushes
        // whatever is pending on its tty when its line editor starts, so input written into the gap
        // between `spawn` and the prompt is discarded — which is a real property of shells rather
        // than anything about this code, and it is why a person never notices: by the time they
        // type, there is a prompt.
        let ready = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < ready {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if t.cells().iter().any(|c| c.grapheme.trim() != "") {
                break;
            }
        }
        t.write(b"exit\r");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !t.finished() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(t.finished(), "and says so when the shell leaves");
    }
}
