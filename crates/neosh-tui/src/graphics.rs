//! Pictures in the transcript, on a terminal that can draw them.
//!
//! The kitty graphics protocol, which is what kitty, Ghostty, WezTerm and Konsole all speak and
//! what tmux will pass through when asked. Nothing else is attempted: a terminal that does not
//! answer the query below gets the picture's *name* on the row instead, which is what every
//! terminal got before this existed and is exactly what a row carrying an image mark still says.
//!
//! # Two ways of saying where
//!
//! A picture is transmitted once, by id, and then *placed*. The protocol has two placement
//! vocabularies and this speaks both, because the terminals split on it:
//!
//! - **Placeholders.** The cells the picture occupies are written as ordinary characters —
//!   `U+10EEEE` with two combining diacritics saying which row and column of the picture each cell
//!   is, and the image id in the foreground colour — and the terminal draws the picture wherever
//!   those cells end up. Which is the right shape for a renderer that thinks in cells: the
//!   picture scrolls with its row, a float over it covers it, a split beside it clips it, and a
//!   frame that changed nothing rewrites nothing, all without a single special case. It is also
//!   the only form that survives tmux, which owns the cells and knows nothing of the pictures.
//!   kitty and Ghostty do this.
//! - **Placements.** The picture is drawn at a screen position, over the cells, and stays there
//!   until deleted. Every frame therefore ends by deleting what was placed and placing what is
//!   now visible — cropped to what a float over it leaves showing, because a placement is drawn
//!   *above* text and a panel opened over a picture would otherwise open under it. WezTerm and
//!   Konsole do this and not the other.
//!
//! Which one is a guess from the environment, confirmed by the query: nothing answers "do you do
//! placeholders", and a terminal that says `OK` to the query and is not known to is given
//! placements, which every implementation has.
//!
//! # What the frontend decides
//!
//! Everything about size. A row carrying [`ExtmarkOpts::image`](neosh_proto::ExtmarkOpts) is one
//! buffer row and says nothing about how tall it draws, because the answer depends on how many
//! pixels a cell is — a fact the terminal reports to the process attached to it (`TIOCGWINSZ`,
//! or `CSI 16 t` for one that does not fill the ioctl in) and to nothing above the terminal
//! boundary. So the row becomes however many screen rows the picture needs, the way a row that
//! wraps does, and the caret, the scroll and the count of rows on screen all read the same list.

use std::collections::HashMap;
use std::io::Write;

use base64::Engine as _;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// The kitty protocol's row and column diacritics, in order: the first is row or column zero.
///
/// Copied from the protocol's own table. Which combining character means which number is the
/// protocol's decision and not something to derive, so this is the list and not a rule.
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

/// The character every placeholder cell is.
const PLACEHOLDER: char = '\u{10EEEE}';

/// A picture bigger than this is not read. The workspace never writes one — it shrinks what it
/// keeps to well under it — so this is a guard against a path that turned out to be something
/// else, not a limit anybody meets.
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// How a terminal is told where a picture goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pictures are names. Every terminal until this existed, and every one that did not answer.
    Off,
    /// Cells that say which part of which picture they are. kitty, Ghostty, and anything under
    /// tmux.
    Placeholders,
    /// A picture drawn at a screen position, re-placed every frame. WezTerm, Konsole.
    Placements,
}

/// What the startup query learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Probe {
    /// Whether the terminal answered the graphics query at all.
    pub graphics: bool,
    /// The pixel size of one cell, if the terminal said (`CSI 16 t`).
    pub cell: Option<(u16, u16)>,
}

/// One picture the terminal has been given, or is about to be.
struct Loaded {
    id: u32,
    width: u32,
    height: u32,
    /// The PNG to send, until it has been sent.
    png: Option<Vec<u8>>,
    /// The virtual placement's size, in placeholder mode — the grid the diacritics index. A
    /// different size is a new placement, so it is kept to know when to make one.
    grid: Option<(u16, u16)>,
}

/// Where a picture fits: its terminal id, and the cells it takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
}

/// One rectangle of a picture on the screen, for the placement vocabulary.
///
/// Rows `k0..k0 + rows` and columns `c0..c0 + cols` of a picture that is `of` cells in all: a
/// picture whose top has scrolled off is placed from its middle, and one a panel half covers is
/// placed in the pieces the panel leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    pub id: u32,
    pub x: u16,
    pub y: u16,
    pub cols: u16,
    pub rows: u16,
    pub k0: u16,
    pub c0: u16,
    pub of: (u16, u16),
    /// Which window this was drawn in, as an index into the paint order, so a later one can be
    /// asked whether it covers it.
    pub layer: usize,
}

pub struct Graphics {
    mode: Mode,
    tmux: bool,
    /// Pixels per cell, as best it is known.
    cell: (u16, u16),
    /// Whether the cell size came from the terminal rather than being assumed.
    cell_known: bool,
    images: HashMap<String, Loaded>,
    next_id: u32,
    /// Escapes to write after the frame: transmissions and, in placeholder mode, placements.
    pending: Vec<String>,
    /// What is placed right now, in placement mode, to know whether this frame changes it.
    placed: Vec<Placed>,
}

impl Graphics {
    /// A terminal that draws no pictures. What every test and every non-terminal frontend gets.
    pub fn off() -> Self {
        Self::new(Mode::Off, false, None)
    }

    pub fn new(mode: Mode, tmux: bool, cell: Option<(u16, u16)>) -> Self {
        Self {
            mode,
            tmux,
            // A cell twice as tall as it is wide is what nearly every monospace face is at any
            // size, so an unknown size distorts nothing visibly: only the ratio is load-bearing.
            cell: cell.unwrap_or((10, 20)),
            cell_known: cell.is_some(),
            images: HashMap::new(),
            next_id: 1,
            pending: Vec::new(),
            placed: Vec::new(),
        }
    }

    /// Decide from the environment and what the query said.
    ///
    /// `NEOSH_NO_IMAGES` turns it off for the reason `NEOSH_NO_ENHANCED_KEYS` exists: this is a
    /// fact about the terminal, decided before any configuration has loaded, and "my terminal
    /// claims to do this and does it wrong" is not something you can fix from inside a program
    /// drawing over your screen.
    pub fn detect(probe: Probe) -> Self {
        if std::env::var_os("NEOSH_NO_IMAGES").is_some() || !probe.graphics {
            return Self::off();
        }
        let env = |k: &str| std::env::var(k).unwrap_or_default();
        let tmux = std::env::var_os("TMUX").is_some();
        let term = env("TERM");
        let program = env("TERM_PROGRAM");
        // Nothing answers "do you do placeholders", so this is the list of terminals known to.
        // Under tmux the answer is placeholders regardless: tmux owns the cells, so a placement
        // aimed at a screen position lands somewhere tmux never told the picture about.
        let placeholders = tmux
            || !env("KITTY_WINDOW_ID").is_empty()
            || term.contains("kitty")
            || term.contains("ghostty")
            || program.eq_ignore_ascii_case("ghostty");
        let mode = if placeholders { Mode::Placeholders } else { Mode::Placements };
        Self::new(mode, tmux, probe.cell.or_else(cell_from_ioctl))
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn enabled(&self) -> bool {
        self.mode != Mode::Off
    }

    pub fn placeholders(&self) -> bool {
        self.mode == Mode::Placeholders
    }

    /// Ask the terminal again how big a cell is. Cheap, and the only way a resize is noticed.
    pub fn refresh_cell(&mut self) {
        if let Some(cell) = cell_from_ioctl() {
            self.cell = cell;
            self.cell_known = true;
        }
    }

    /// Where `path` fits in `cols` columns and at most `max_rows` rows, loading it on first sight.
    ///
    /// `None` when there is nothing to draw: pictures are off, the file cannot be read or is not
    /// a picture, or there is no room. Never scaled *up* — a small icon is a small icon — and
    /// scaled down to fit either bound with its aspect ratio kept, because the placement stretches
    /// to whatever cell box it is given and the box is what has to be right.
    pub fn fit(&mut self, path: &str, cols: u16, max_rows: u16) -> Option<Fit> {
        if !self.enabled() || cols == 0 || max_rows == 0 {
            return None;
        }
        let (cw, ch) = (f64::from(self.cell.0.max(1)), f64::from(self.cell.1.max(1)));
        let (id, w, h) = {
            let loaded = self.load(path)?;
            (loaded.id, f64::from(loaded.width), f64::from(loaded.height))
        };
        // Whole cells, so a picture that is one pixel wider than a column takes the next column
        // rather than losing that pixel.
        let natural_cols = (w / cw).ceil().max(1.0);
        let natural_rows = (h / ch).ceil().max(1.0);
        let scale = (f64::from(cols) / natural_cols)
            .min(f64::from(max_rows) / natural_rows)
            .min(1.0);
        let c = (natural_cols * scale).floor().max(1.0);
        // Rows from the columns actually taken, not from the scale: the columns were rounded, and
        // the rows have to agree with them or the picture is squashed by the rounding.
        let r = (c * cw * h / (w * ch)).round().clamp(1.0, f64::from(max_rows));
        let limit = DIACRITICS.len() as f64;
        let (c, r) = (c.min(limit) as u16, r.min(limit) as u16);
        if self.placeholders() {
            self.ensure_grid(path, id, c, r);
        }
        Some(Fit { id, cols: c, rows: r })
    }

    /// Load a file the first time it is asked about.
    fn load(&mut self, path: &str) -> Option<&Loaded> {
        if !self.images.contains_key(path) {
            let loaded = read_png(path).map(|(png, width, height)| {
                let id = self.next_id;
                self.next_id += 1;
                Loaded { id, width, height, png: Some(png), grid: None }
            });
            match loaded {
                Some(l) => {
                    self.images.insert(path.to_string(), l);
                }
                None => return None,
            }
        }
        self.images.get(path)
    }

    /// Make sure the terminal has a virtual placement of this size for the picture.
    fn ensure_grid(&mut self, path: &str, id: u32, cols: u16, rows: u16) {
        let Some(loaded) = self.images.get_mut(path) else { return };
        if loaded.grid == Some((cols, rows)) {
            return;
        }
        let transmitted = loaded.png.is_none();
        if let Some(png) = loaded.png.take() {
            // The first time: transmit and place in one, which is the `T` action with `U=1`.
            self.pending.extend(transmit(
                &png,
                &format!("a=T,U=1,i={id},f=100,c={cols},r={rows},q=2"),
            ));
        } else if transmitted {
            // A new size is a new virtual placement. The old one goes — `d=i` is the deletion
            // that reaches virtual placements, and lowercase keeps the pixels.
            self.pending.push(format!("\x1b_Ga=d,d=i,i={id},q=2\x1b\\"));
            self.pending.push(format!("\x1b_Ga=p,U=1,i={id},c={cols},r={rows},q=2\x1b\\"));
        }
        if let Some(loaded) = self.images.get_mut(path) {
            loaded.grid = Some((cols, rows));
        }
    }

    /// One placeholder cell: row `k`, column `col` of picture `id`.
    ///
    /// Both diacritics on every cell. The protocol lets a cell inherit them from its neighbour,
    /// but a cell that has to be read against the one before it is a cell that goes wrong the
    /// moment the renderer rewrites only the ones that changed — which is what a renderer does.
    pub fn cell(&self, id: u32, k: u16, col: u16) -> Span<'static> {
        let mut s = String::with_capacity(12);
        s.push(PLACEHOLDER);
        if let Some(c) = DIACRITICS.get(k as usize).and_then(|&c| char::from_u32(c)) {
            s.push(c);
        }
        if let Some(c) = DIACRITICS.get(col as usize).and_then(|&c| char::from_u32(c)) {
            s.push(c);
        }
        // The id in the foreground: eight bits as a palette index while it fits, which every
        // colour depth carries, and twenty-four otherwise.
        let fg = if id < 256 {
            Color::Indexed(id as u8)
        } else {
            Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
        };
        Span::styled(s, Style::default().fg(fg))
    }

    /// Whether a symbol is a placeholder cell — which the block caret must not be painted over,
    /// because reversing its colours would swap the image id for the background.
    pub fn is_placeholder(symbol: &str) -> bool {
        symbol.starts_with(PLACEHOLDER)
    }

    /// What this frame placed, in placement mode. Compared with the last frame: only a change
    /// costs escapes, so a frame that only moved the caret writes nothing.
    pub fn commit(&mut self, placed: &[Placed]) {
        if self.mode != Mode::Placements {
            return;
        }
        let mut now: Vec<Placed> = placed.to_vec();
        now.sort_unstable_by_key(|p| (p.id, p.y, p.x, p.k0, p.c0, p.rows, p.cols));
        if now == self.placed {
            return;
        }
        // Everything on screen goes and what is visible comes back, in one write: the terminal
        // shows the result of the whole sequence, not the gap in the middle of it.
        self.pending.push("\x1b_Ga=d,d=a,q=2\x1b\\".to_string());
        for p in placed {
            let Some(loaded) = self.images.values().find(|l| l.id == p.id) else { continue };
            // The rows showing, as a band of the source: the picture is `of.1` rows tall in all,
            // and this placement is rows `k0..k0 + rows` of it.
            let per_row = f64::from(loaded.height) / f64::from(p.of.1.max(1));
            let y = (f64::from(p.k0) * per_row).round() as u32;
            let h = ((f64::from(p.rows) * per_row).round() as u32).max(1).min(loaded.height.saturating_sub(y).max(1));
            let per_col = f64::from(loaded.width) / f64::from(p.of.0.max(1));
            let x = (f64::from(p.c0) * per_col).round() as u32;
            let w = ((f64::from(p.cols) * per_col).round() as u32).max(1).min(loaded.width.saturating_sub(x).max(1));
            self.pending.push(format!(
                "\x1b[{};{}H\x1b_Ga=p,i={},x={x},y={y},w={w},h={h},c={},r={},C=1,q=2\x1b\\",
                p.y + 1,
                p.x + 1,
                p.id,
                p.cols,
                p.rows,
            ));
        }
        self.placed = now;
    }

    /// Transmit what has not been transmitted, in placement mode. Placeholder mode transmits when
    /// it makes the grid, since the two are one command there.
    pub fn transmit_pending(&mut self) {
        if self.mode != Mode::Placements {
            return;
        }
        for loaded in self.images.values_mut() {
            if let Some(png) = loaded.png.take() {
                self.pending.extend(transmit(&png, &format!("a=t,i={},f=100,q=2", loaded.id)));
            }
        }
    }

    /// Write whatever this frame owes the terminal.
    pub fn flush(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        for seq in self.pending.drain(..) {
            if self.tmux {
                out.write_all(wrap_tmux(&seq).as_bytes())?;
            } else {
                out.write_all(seq.as_bytes())?;
            }
        }
        out.flush()
    }

    /// The escape that takes every picture back out of the terminal, for the way out.
    ///
    /// Uppercase: the pixels go too. A terminal keeps what it was sent until told otherwise, and
    /// a shell inheriting a hundred megabytes of somebody else's screenshots is a leak with no
    /// owner.
    pub fn farewell(&self) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        let seq = "\x1b_Ga=d,d=A,q=2\x1b\\";
        Some(if self.tmux { wrap_tmux(seq) } else { seq.to_string() })
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> &[String] {
        &self.pending
    }

    #[cfg(test)]
    pub(crate) fn cell_known(&self) -> bool {
        self.cell_known
    }
}

/// What of a placement is left showing once the windows painted after it are taken away.
///
/// A placement is drawn over the cells, so a float opened over a picture would otherwise open
/// under it: the pieces around the float are placed and the piece beneath it is not. Rectangles
/// only — a placement is one rectangle of source pixels scaled into one rectangle of cells — so a
/// window carved out of the middle leaves up to four of them, and a second window carves each of
/// those again.
pub fn uncovered(p: Placed, later: &[(u16, u16, u16, u16)]) -> Vec<Placed> {
    let mut pieces = vec![(p.x, p.y, p.cols, p.rows)];
    for &(bx, by, bw, bh) in later {
        if bw == 0 || bh == 0 {
            continue;
        }
        let mut next = Vec::new();
        for (ax, ay, aw, ah) in pieces {
            let (ax2, ay2, bx2, by2) = (ax + aw, ay + ah, bx + bw, by + bh);
            if bx >= ax2 || bx2 <= ax || by >= ay2 || by2 <= ay {
                next.push((ax, ay, aw, ah));
                continue;
            }
            // Above and below the cover, full width; then left and right of it, between.
            if by > ay {
                next.push((ax, ay, aw, by - ay));
            }
            if by2 < ay2 {
                next.push((ax, by2, aw, ay2 - by2));
            }
            let (my, my2) = (by.max(ay), by2.min(ay2));
            if bx > ax {
                next.push((ax, my, bx - ax, my2 - my));
            }
            if bx2 < ax2 {
                next.push((bx2, my, ax2 - bx2, my2 - my));
            }
        }
        pieces = next;
    }
    pieces
        .into_iter()
        .map(|(x, y, cols, rows)| Placed {
            x,
            y,
            cols,
            rows,
            k0: p.k0 + (y - p.y),
            c0: p.c0 + (x - p.x),
            ..p
        })
        .collect()
}

/// A file as a PNG, with its size.
///
/// PNG is the one encoded form the protocol takes, so a JPEG, GIF or WebP is decoded and written
/// back out — a copy the terminal receives once and nothing keeps. A PNG goes as it is: decoding
/// it only to re-encode it would cost a second of CPU on a large screenshot and buy nothing.
fn read_png(path: &str) -> Option<(Vec<u8>, u32, u32)> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let reader = image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format().ok()?;
    let format = reader.format();
    if format == Some(image::ImageFormat::Png) {
        let (w, h) = reader.into_dimensions().ok()?;
        return Some((bytes, w, h));
    }
    let decoded = reader.decode().ok()?;
    let (w, h) = image::GenericImageView::dimensions(&decoded);
    let mut png = Vec::new();
    decoded.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    Some((png, w, h))
}

/// A transmission, chunked the way the protocol wants it.
///
/// Four kilobytes of base64 per escape, `m=1` on every chunk but the last, and the keys that
/// describe the picture on the first alone.
fn transmit(png: &[u8], first_keys: &str) -> Vec<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    let n = chunks.len();
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let more = usize::from(i + 1 < n);
            let payload = std::str::from_utf8(chunk).unwrap_or("");
            if i == 0 {
                format!("\x1b_G{first_keys},m={more};{payload}\x1b\\")
            } else {
                format!("\x1b_Gm={more},q=2;{payload}\x1b\\")
            }
        })
        .collect()
}

/// The tmux passthrough, per escape: tmux forwards nothing it does not understand, and a picture
/// is several thousand escapes it does not understand.
fn wrap_tmux(seq: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
}

/// How many pixels a cell is, from the terminal's own report.
///
/// `TIOCGWINSZ` carries it beside the row and column counts, and kitty, Ghostty, WezTerm and
/// recent tmux all fill it in. Zero is "did not say", which is what the fallback query is for.
#[cfg(unix)]
fn cell_from_ioctl() -> Option<(u16, u16)> {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // Stdout is the stream the frame is drawn on, so it is the terminal whose cells matter.
    // SAFETY: `TIOCGWINSZ` writes a `winsize` and nothing else, and one is provided.
    let rc = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 || ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    Some((ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

#[cfg(not(unix))]
fn cell_from_ioctl() -> Option<(u16, u16)> {
    None
}

/// Ask the terminal, before anything else reads from it, whether it draws pictures and how big a
/// cell is.
///
/// Three queries in one write — the graphics query, `CSI 16 t` for the cell size, and the primary
/// device attributes — and the last is the one every terminal answers, so its reply is what says
/// the others are not coming. Read straight off the descriptor: crossterm's reader has not been
/// started yet, and must not be, because it would take the reply for keystrokes and a reply
/// typed into the composer is the one outcome worse than not asking.
///
/// Bounded, so a terminal that answers nothing — a pty in a test, an old emulator — costs at most
/// the wait and never a hang. And skipped altogether under `NEOSH_NO_IMAGES` or `TERM=dumb`.
#[cfg(unix)]
pub fn probe(out: &mut impl Write) -> Probe {
    use std::time::{Duration, Instant};
    if std::env::var_os("NEOSH_NO_IMAGES").is_some()
        || std::env::var("TERM").map(|t| t.is_empty() || t == "dumb").unwrap_or(true)
    {
        return Probe::default();
    }
    let query = "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
    let tmux = std::env::var_os("TMUX").is_some();
    let query = if tmux { wrap_tmux(query) } else { query.to_string() };
    if out.write_all(format!("{query}\x1b[16t\x1b[c").as_bytes()).is_err() || out.flush().is_err()
    {
        return Probe::default();
    }
    let deadline = Instant::now() + Duration::from_millis(600);
    let mut seen = Vec::new();
    let mut buf = [0u8; 256];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        let mut fds = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd, and the count says so.
        let ready = unsafe { libc::poll(&mut fds, 1, left.as_millis() as i32) };
        if ready <= 0 {
            break;
        }
        // SAFETY: the buffer is as long as the count says.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        seen.extend_from_slice(&buf[..n as usize]);
        if parse_probe(&seen).1 {
            break;
        }
    }
    parse_probe(&seen).0
}

#[cfg(not(unix))]
pub fn probe(_out: &mut impl Write) -> Probe {
    Probe::default()
}

/// What the replies so far say, and whether the device-attributes reply — the last one — is in.
fn parse_probe(seen: &[u8]) -> (Probe, bool) {
    let text = String::from_utf8_lossy(seen);
    let graphics = text.contains("\x1b_Gi=31;OK");
    // `CSI 6 ; height ; width t`.
    let cell = text.find("\x1b[6;").and_then(|at| {
        let rest = &text[at + 4..];
        let end = rest.find('t')?;
        let mut parts = rest[..end].split(';');
        let h: u16 = parts.next()?.parse().ok()?;
        let w: u16 = parts.next()?.parse().ok()?;
        (w > 0 && h > 0).then_some((w, h))
    });
    // `CSI ? … c`, which is the only thing here that starts with `CSI ?` and ends with `c`.
    let done = text.find("\x1b[?").is_some_and(|at| text[at..].contains('c'));
    (Probe { graphics, cell }, done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One-pixel PNG.
    const PIXEL: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0x60, 0xf8, 0x5f, 0x0f, 0x00, 0x02, 0x87, 0x01, 0x80, 0xeb, 0x47, 0xba, 0x92, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn png_file(name: &str, w: u32, h: u32) -> String {
        let path = std::env::temp_dir().join(format!("neosh-gfx-{}-{name}.png", std::process::id()));
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode");
        std::fs::write(&path, bytes).expect("write");
        path.display().to_string()
    }

    #[test]
    fn the_table_starts_where_the_protocol_says_it_does() {
        assert_eq!(DIACRITICS[0], 0x305, "zero");
        assert_eq!(DIACRITICS[1], 0x30D, "one");
        let g = Graphics::new(Mode::Placeholders, false, Some((10, 20)));
        let cell = g.cell(7, 1, 0);
        let mut chars = cell.content.chars();
        assert_eq!(chars.next(), Some(PLACEHOLDER));
        assert_eq!(chars.next(), Some('\u{30D}'), "row one");
        assert_eq!(chars.next(), Some('\u{305}'), "column zero");
        assert_eq!(cell.style.fg, Some(Color::Indexed(7)));
        // One column on screen, whatever the diacritics are: a cell that measured two would push
        // the rest of the row along and the picture with it.
        assert_eq!(unicode_width::UnicodeWidthStr::width(cell.content.as_ref()), 1);
    }

    /// A picture is scaled down to what fits and never up, with the box in cells matching the
    /// picture's shape — because the terminal stretches to the box, so the box is what has to be
    /// right.
    #[test]
    fn a_picture_fits_its_column_with_its_shape_kept() {
        let wide = png_file("wide", 400, 100);
        let mut g = Graphics::new(Mode::Placeholders, false, Some((10, 20)));
        // 40 columns natural, 5 rows natural. Room for all of it.
        assert_eq!(g.fit(&wide, 80, 30), Some(Fit { id: 1, cols: 40, rows: 5 }));
        // Half the width: half the rows, rounded.
        assert_eq!(g.fit(&wide, 20, 30), Some(Fit { id: 1, cols: 20, rows: 3 }));
        // Bounded by rows: two rows is eight columns of a 4:1 picture at a 1:2 cell.
        assert_eq!(g.fit(&wide, 80, 2), Some(Fit { id: 1, cols: 16, rows: 2 }));
        let tiny = png_file("tiny", 8, 8);
        assert_eq!(g.fit(&tiny, 80, 30), Some(Fit { id: 2, cols: 1, rows: 1 }), "never enlarged");
        let _ = std::fs::remove_file(wide);
        let _ = std::fs::remove_file(tiny);
    }

    /// The first fit transmits and places in one; a new size is a new placement and no second
    /// copy of the pixels; the same size again costs nothing.
    #[test]
    fn a_picture_is_sent_once_and_replaced_when_its_size_changes() {
        let p = png_file("once", 100, 100);
        let mut g = Graphics::new(Mode::Placeholders, false, Some((10, 20)));
        g.fit(&p, 80, 30);
        let first = g.pending().to_vec();
        assert_eq!(first.len(), 1, "one chunk for a small file: {first:?}");
        assert!(first[0].starts_with("\x1b_Ga=T,U=1,i=1,f=100,c=10,r=5,q=2,m=0;"), "{}", first[0]);
        g.pending.clear();
        g.fit(&p, 80, 30);
        assert!(g.pending().is_empty(), "nothing to say twice");
        g.fit(&p, 4, 30);
        assert_eq!(
            g.pending(),
            &["\x1b_Ga=d,d=i,i=1,q=2\x1b\\", "\x1b_Ga=p,U=1,i=1,c=4,r=2,q=2\x1b\\"]
        );
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn a_transmission_is_chunked_the_way_the_protocol_wants() {
        let big = vec![0u8; 9000];
        let chunks = transmit(&big, "a=t,i=3,f=100,q=2");
        assert_eq!(chunks.len(), 3);
        assert!(chunks[0].starts_with("\x1b_Ga=t,i=3,f=100,q=2,m=1;"));
        assert!(chunks[1].starts_with("\x1b_Gm=1,q=2;"));
        assert!(chunks[2].starts_with("\x1b_Gm=0,q=2;"));
        for c in &chunks[..2] {
            let payload = c.split(';').nth(1).map(|p| p.trim_end_matches("\x1b\\").len());
            assert_eq!(payload, Some(4096), "{c:?}");
        }
    }

    /// Placements are re-said only when something moved, and a picture whose top has scrolled
    /// off is placed from the row that is showing.
    #[test]
    fn placements_change_only_when_the_screen_does() {
        let p = png_file("place", 100, 200);
        let mut g = Graphics::new(Mode::Placements, false, Some((10, 20)));
        let fit = g.fit(&p, 80, 30).expect("fits");
        assert_eq!((fit.cols, fit.rows), (10, 10));
        g.transmit_pending();
        assert!(g.pending()[0].starts_with("\x1b_Ga=t,i=1,f=100,q=2,m=0;"));
        g.pending.clear();
        let whole = Placed { id: 1, x: 2, y: 3, cols: 10, rows: 10, k0: 0, c0: 0, of: (10, 10), layer: 0 };
        g.commit(&[whole]);
        assert_eq!(g.pending().len(), 2, "delete all, place one");
        assert_eq!(g.pending()[1], "\x1b[4;3H\x1b_Ga=p,i=1,x=0,y=0,w=100,h=200,c=10,r=10,C=1,q=2\x1b\\");
        g.pending.clear();
        g.commit(&[whole]);
        assert!(g.pending().is_empty(), "the same frame says nothing");
        let half = Placed { id: 1, x: 2, y: 0, cols: 10, rows: 4, k0: 6, c0: 0, of: (10, 10), layer: 0 };
        g.commit(&[half]);
        assert_eq!(g.pending()[1], "\x1b[1;3H\x1b_Ga=p,i=1,x=0,y=120,w=100,h=80,c=10,r=4,C=1,q=2\x1b\\");
        g.pending.clear();
        // The right half of the middle rows, which is what a panel over the left leaves.
        let corner = Placed { id: 1, x: 7, y: 5, cols: 5, rows: 2, k0: 2, c0: 5, of: (10, 10), layer: 0 };
        g.commit(&[corner]);
        assert_eq!(g.pending()[1], "\x1b[6;8H\x1b_Ga=p,i=1,x=50,y=40,w=50,h=40,c=5,r=2,C=1,q=2\x1b\\");
        let _ = std::fs::remove_file(p);
    }

    /// A panel over the middle of a picture leaves four pieces, each cropped to its own part.
    #[test]
    fn a_window_over_a_picture_leaves_the_pieces_around_it() {
        let whole = Placed { id: 1, x: 0, y: 0, cols: 10, rows: 10, k0: 0, c0: 0, of: (10, 10), layer: 0 };
        assert_eq!(uncovered(whole, &[(20, 20, 5, 5)]), vec![whole], "nothing over it");
        let pieces = uncovered(whole, &[(3, 4, 4, 2)]);
        assert_eq!(pieces.len(), 4);
        assert_eq!((pieces[0].y, pieces[0].rows, pieces[0].k0), (0, 4, 0), "above");
        assert_eq!((pieces[1].y, pieces[1].rows, pieces[1].k0), (6, 4, 6), "below");
        assert_eq!((pieces[2].x, pieces[2].cols, pieces[2].c0, pieces[2].y, pieces[2].rows), (0, 3, 0, 4, 2), "left");
        assert_eq!((pieces[3].x, pieces[3].cols, pieces[3].c0), (7, 3, 7), "right");
        assert!(uncovered(whole, &[(0, 0, 10, 10)]).is_empty(), "entirely covered");
    }

    #[test]
    fn the_probe_reads_both_answers_and_knows_when_the_last_is_in() {
        let (p, done) = parse_probe(b"\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t");
        assert!(p.graphics);
        assert_eq!(p.cell, Some((10, 20)));
        assert!(!done, "the device attributes reply is what ends the wait");
        let (p, done) = parse_probe(b"\x1b[?62;22c");
        assert!(!p.graphics && p.cell.is_none() && done, "a terminal that only did the last");
    }

    #[test]
    fn off_draws_nothing_and_a_file_that_is_not_a_picture_is_not_one() {
        let mut off = Graphics::off();
        let p = png_file("off", 10, 10);
        assert_eq!(off.fit(&p, 80, 30), None);
        let mut on = Graphics::new(Mode::Placeholders, false, None);
        assert!(!on.cell_known());
        let text = std::env::temp_dir().join(format!("neosh-gfx-{}-text.png", std::process::id()));
        std::fs::write(&text, b"not a picture").expect("write");
        assert_eq!(on.fit(&text.display().to_string(), 80, 30), None);
        assert!(on.fit("/nowhere/at/all.png", 80, 30).is_none());
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(text);
        let _ = PIXEL;
    }

    #[test]
    fn under_tmux_every_escape_is_handed_through() {
        let mut g = Graphics::new(Mode::Placeholders, true, Some((10, 20)));
        g.pending.push("\x1b_Ga=d,d=i,i=1,q=2\x1b\\".into());
        let mut out = Vec::new();
        g.flush(&mut out).expect("write");
        assert_eq!(out, b"\x1bPtmux;\x1b\x1b_Ga=d,d=i,i=1,q=2\x1b\x1b\\\x1b\\");
        assert!(g.farewell().expect("on").starts_with("\x1bPtmux;"));
        assert!(Graphics::off().farewell().is_none());
    }
}
