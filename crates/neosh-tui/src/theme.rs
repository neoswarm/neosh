//! Highlight resolution and colour down-conversion.
//!
//! Plugins declare semantic group names and, at most, truecolor values. Everything about what a
//! given terminal can actually display is decided here, at the boundary — a plugin that reasons
//! about 256-colour fallbacks is a plugin that renders wrong on somebody else's terminal.

use std::collections::BTreeMap;

use neosh_proto::{Attrs, Color, HighlightDef, HighlightSpec};
use ratatui::style::{Color as TuiColor, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorDepth {
    TrueColor,
    Ansi256,
    Ansi16,
}

impl ColorDepth {
    /// Infer what the terminal supports.
    ///
    /// `COLORTERM` is the only reliable truecolor signal; `TERM` distinguishes 256 from 16. When in
    /// doubt this errs downward, because too-few colours look plain while too-many look corrupt.
    pub fn detect() -> Self {
        let colorterm = std::env::var("COLORTERM").unwrap_or_default();
        if colorterm.contains("truecolor") || colorterm.contains("24bit") {
            return Self::TrueColor;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if term.contains("256") || term.contains("direct") {
            Self::Ansi256
        } else {
            Self::Ansi16
        }
    }
}

/// How deep a link chain may go before we call it a cycle. Matches the core's limit.
const MAX_LINK_DEPTH: usize = 16;

#[derive(Debug, Clone)]
pub struct Theme {
    groups: BTreeMap<String, HighlightDef>,
    depth: ColorDepth,
    /// Whether animated groups actually animate. Follows `ui.motion`.
    ///
    /// Held here rather than checked at each call site so that turning motion off is one decision
    /// in one place — and so a group that animates still renders its colours when it is off, rather
    /// than disappearing along with the movement.
    motion: bool,
}

impl Theme {
    pub fn new(depth: ColorDepth) -> Self {
        Self { groups: BTreeMap::new(), depth, motion: true }
    }

    pub fn set_motion(&mut self, on: bool) {
        self.motion = on;
    }

    pub fn motion(&self) -> bool {
        self.motion
    }

    /// Whether a blend between two colours will survive the trip to the terminal.
    pub fn truecolor(&self) -> bool {
        self.depth == ColorDepth::TrueColor
    }

    pub fn set(&mut self, name: impl Into<String>, def: HighlightDef) {
        self.groups.insert(name.into(), def);
    }

    pub fn sync(&mut self, groups: &BTreeMap<String, HighlightDef>) {
        self.groups = groups.clone();
    }

    pub fn depth(&self) -> ColorDepth {
        self.depth
    }

    /// Resolve a group name to a concrete style.
    ///
    /// An unknown group, or a cyclic link chain, resolves to the default style rather than an
    /// error: a plugin typo should look plain, not crash the renderer or hang it.
    pub fn style(&self, name: &str) -> Style {
        let Some(spec) = self.resolve(name) else { return Style::default() };
        self.to_style(&spec)
    }

    /// Whether this group resolves to anything at all.
    ///
    /// The one caller that needs to know is the block caret: `style` answers "plain" for a group
    /// nobody defined, which for a caret is the same as not drawing one. A theme that has never
    /// heard of `Cursor` should get a visible cursor anyway — a palette entry is how you change
    /// what it looks like, not whether there is one.
    pub fn defines(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }

    /// The animation a group carries, if any. Follows links, like the colours do.
    pub fn animation(&self, name: &str) -> Option<neosh_proto::Animation> {
        self.resolve(name).and_then(|s| s.animate)
    }

    fn resolve(&self, name: &str) -> Option<HighlightSpec> {
        let mut cur = name;
        for _ in 0..MAX_LINK_DEPTH {
            match self.groups.get(cur)? {
                HighlightDef::Spec { spec } => return Some(*spec),
                HighlightDef::Link { to } => cur = to,
            }
        }
        None
    }

    fn to_style(&self, spec: &HighlightSpec) -> Style {
        let mut s = Style::default();
        if let Some(fg) = spec.fg {
            s = s.fg(self.color(fg));
        }
        if let Some(bg) = spec.bg {
            s = s.bg(self.color(bg));
        }
        s.add_modifier(modifiers(&spec.attrs))
    }

    pub fn color(&self, c: Color) -> TuiColor {
        match (c, self.depth) {
            (Color::Default, _) => TuiColor::Reset,
            (Color::Rgb { r, g, b }, ColorDepth::TrueColor) => TuiColor::Rgb(r, g, b),
            (Color::Rgb { r, g, b }, ColorDepth::Ansi256) => TuiColor::Indexed(rgb_to_256(r, g, b)),
            (Color::Rgb { r, g, b }, ColorDepth::Ansi16) => {
                ansi16(indexed_to_16(rgb_to_256(r, g, b)))
            }
            (Color::Indexed { i }, ColorDepth::Ansi16) => ansi16(indexed_to_16(i)),
            (Color::Indexed { i }, _) => TuiColor::Indexed(i),
        }
    }
}

pub fn modifiers(a: &Attrs) -> Modifier {
    let mut m = Modifier::empty();
    if a.bold {
        m |= Modifier::BOLD;
    }
    if a.italic {
        m |= Modifier::ITALIC;
    }
    if a.underline {
        m |= Modifier::UNDERLINED;
    }
    if a.reverse {
        m |= Modifier::REVERSED;
    }
    if a.dim {
        m |= Modifier::DIM;
    }
    if a.strikethrough {
        m |= Modifier::CROSSED_OUT;
    }
    m
}

/// Map 24-bit RGB onto the xterm-256 palette.
///
/// Greys are matched against the 24-step ramp first: quantizing a near-grey into the 6×6×6 cube
/// produces a visible colour cast, which is the most noticeable artifact of naive down-conversion.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let max = r.max(g).max(b) as i32;
    let min = r.min(g).min(b) as i32;
    if max - min < 8 {
        let level = ((r as u32 + g as u32 + b as u32) / 3) as u8;
        if level < 8 {
            return 16;
        }
        if level > 248 {
            return 231;
        }
        return 232 + ((level as u32 - 8) * 24 / 240) as u8;
    }
    let q = |v: u8| -> u32 {
        // Cube levels are 0, 95, 135, 175, 215, 255 — not evenly spaced.
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| (**l as i32 - v as i32).abs())
            .map(|(i, _)| i as u32)
            .unwrap_or(0)
    };
    (16 + 36 * q(r) + 6 * q(g) + q(b)) as u8
}

/// Collapse a 256-colour index to the 16 basic ANSI colours.
pub fn indexed_to_16(i: u8) -> u8 {
    match i {
        0..=15 => i,
        16..=231 => {
            let n = i as u32 - 16;
            let (r, g, b) = (n / 36, (n / 6) % 6, n % 6);
            let bright = r.max(g).max(b) >= 3;
            let bit = |v: u32| u8::from(v >= 3);
            let base = bit(r) | (bit(g) << 1) | (bit(b) << 2);
            // ANSI orders red/green/blue by bit; remap from the cube's ordering.
            let ansi = (base & 0b001) | (base & 0b010) | (base & 0b100);
            if bright { ansi + 8 } else { ansi }
        }
        // The greyscale ramp: dark greys read as black, light as white.
        232..=243 => 0,
        244..=250 => 8,
        _ => 7,
    }
}

fn ansi16(i: u8) -> TuiColor {
    match i {
        0 => TuiColor::Black,
        1 => TuiColor::Red,
        2 => TuiColor::Green,
        3 => TuiColor::Yellow,
        4 => TuiColor::Blue,
        5 => TuiColor::Magenta,
        6 => TuiColor::Cyan,
        7 => TuiColor::Gray,
        8 => TuiColor::DarkGray,
        9 => TuiColor::LightRed,
        10 => TuiColor::LightGreen,
        11 => TuiColor::LightYellow,
        12 => TuiColor::LightBlue,
        13 => TuiColor::LightMagenta,
        14 => TuiColor::LightCyan,
        _ => TuiColor::White,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(depth: ColorDepth) -> Theme {
        let mut t = Theme::new(depth);
        t.set("Base", HighlightDef::Spec {
            spec: HighlightSpec {
                fg: Some(Color::Rgb { r: 200, g: 30, b: 30 }),
                bg: None,
                attrs: Attrs { bold: true, ..Default::default() },
                animate: None,
            },
        });
        t
    }

    #[test]
    fn truecolor_passes_rgb_through_untouched() {
        let s = theme(ColorDepth::TrueColor).style("Base");
        assert_eq!(s.fg, Some(TuiColor::Rgb(200, 30, 30)));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_limited_terminal_gets_an_index_not_rgb() {
        assert!(matches!(
            theme(ColorDepth::Ansi256).style("Base").fg,
            Some(TuiColor::Indexed(_))
        ));
        assert!(!matches!(theme(ColorDepth::Ansi16).style("Base").fg, Some(TuiColor::Rgb(..))));
    }

    #[test]
    fn near_greys_use_the_grey_ramp_rather_than_the_colour_cube() {
        // A naive cube quantization tints these; the ramp keeps them neutral.
        for v in [40u8, 90, 128, 200] {
            let idx = rgb_to_256(v, v, v);
            assert!((232..=255).contains(&idx), "{v} -> {idx} should be on the grey ramp");
        }
    }

    #[test]
    fn pure_black_and_white_land_on_the_cube_corners() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
    }

    #[test]
    fn saturated_colours_do_not_collapse_to_grey() {
        let red = rgb_to_256(255, 0, 0);
        assert!(!(232..=255).contains(&red), "red must not be quantized to the grey ramp");
    }

    #[test]
    fn a_link_chain_resolves_and_a_cycle_does_not_hang() {
        let mut t = theme(ColorDepth::TrueColor);
        t.set("A", HighlightDef::Link { to: "B".into() });
        t.set("B", HighlightDef::Link { to: "Base".into() });
        assert_eq!(t.style("A").fg, Some(TuiColor::Rgb(200, 30, 30)));

        t.set("X", HighlightDef::Link { to: "Y".into() });
        t.set("Y", HighlightDef::Link { to: "X".into() });
        assert_eq!(t.style("X"), Style::default(), "a cycle degrades to plain, not a hang");
    }

    #[test]
    fn an_unknown_group_renders_plain_instead_of_failing() {
        assert_eq!(theme(ColorDepth::TrueColor).style("Nope"), Style::default());
    }

    #[test]
    fn every_256_index_maps_into_the_16_colour_range() {
        for i in 0..=255u8 {
            assert!(indexed_to_16(i) <= 15, "index {i} escaped the 16-colour range");
        }
    }
}
