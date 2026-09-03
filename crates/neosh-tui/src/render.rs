//! Layout resolution and drawing.
//!
//! # Where display-width math lives
//!
//! Here, and nowhere else. The protocol carries UTF-8 *byte* offsets, so the core never touches
//! `unicode-width` and cannot get column arithmetic wrong. Converting a byte offset to a screen
//! column is a single function ([`display_col`]) that every caller goes through, which is what
//! keeps CJK and emoji from corrupting layout the first time an agent emits them.
//!
//! # Where layout happens
//!
//! Also here. The core sends declarative geometry — a dock, or an anchor plus a preferred size —
//! and [`resolve_layout`] turns that into rectangles against a real viewport. That is the split
//! that lets a web frontend reuse the entire protocol.

use neosh_proto::{
    Anchor, CursorShape, Dock, Extent, LineRender, VirtTextPos, WindowId, WindowLayout,
};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};
use ratatui::Frame;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::mirror::Mirror;
use crate::theme::Theme;

/// Screen columns occupied by `text`.
pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Convert a UTF-8 byte offset into a screen column.
///
/// Clamps to the nearest character boundary at or below `byte_col`, so a desynced or mid-codepoint
/// offset degrades to a slightly-left position rather than panicking.
pub fn display_col(text: &str, byte_col: usize) -> usize {
    let mut b = byte_col.min(text.len());
    while b > 0 && !text.is_char_boundary(b) {
        b -= 1;
    }
    width(&text[..b])
}

/// Truncate to at most `max` screen columns without splitting a grapheme cluster.
pub fn truncate_to_width(text: &str, max: usize) -> &str {
    if width(text) <= max {
        return text;
    }
    let mut used = 0;
    let mut end = 0;
    for (i, g) in text.grapheme_indices(true) {
        let w = width(g);
        if used + w > max {
            end = i;
            break;
        }
        used += w;
        end = i + g.len();
    }
    &text[..end]
}

// ---------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------

/// Break one rendered line into as many screen lines as it needs.
///
/// Not `Paragraph::wrap`: that reflows whitespace and rebuilds spans, which loses the correspondence
/// between a span and the extmark that produced it — so a highlight ends up on the wrong columns the
/// first time a line is long enough to matter. This splits at grapheme-cluster boundaries inside
/// each span, carries the style across the break, and never splits a cluster.
///
/// Width is clamped to at least one column. A cluster too wide for that column — CJK at width 1,
/// an emoji sequence in any narrow pane — still cannot fit, so it overflows onto a line of its
/// own instead of being dropped. Every pass through the loop consumes at least one cluster, which
/// is what stops a too-narrow pane from looping forever and exhausting memory.
pub fn wrap_line(line: &Line<'static>, width_cols: usize) -> Vec<Line<'static>> {
    let limit = width_cols.max(1);
    let total: usize = line.spans.iter().map(|s| width(&s.content)).sum();
    if total <= limit {
        return vec![line.clone()];
    }
    // Whatever the row as a whole is styled with — the band under a changed diff line — belongs to
    // every screen row it wraps onto. A green line that goes back to the terminal background
    // halfway down reads as two lines, one of which changed.
    let base = line.style;

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;

    for span in &line.spans {
        let style = span.style;
        let mut text = span.content.as_ref();
        while !text.is_empty() {
            let room = limit - used;
            if room == 0 {
                out.push(Line::from(std::mem::take(&mut current)).style(base));
                used = 0;
                continue;
            }
            let head = truncate_to_width(text, room);
            if head.is_empty() {
                if used > 0 {
                    // Something is on this line already — break, then retry the cluster against
                    // a full-width line. `used` drops to 0, so the next pass has more room.
                    out.push(Line::from(std::mem::take(&mut current)).style(base));
                    used = 0;
                    continue;
                }
                // The line is already empty and the cluster still does not fit, so it is wider
                // than `limit` and no amount of breaking will ever make room for it. Give it a
                // line of its own and advance past it: flushing again would leave `text`, `used`
                // and `current` exactly as they are, and the loop would never terminate.
                let cluster = text.graphemes(true).next().unwrap_or(text);
                out.push(Line::from(Span::styled(cluster.to_string(), style)).style(base));
                text = &text[cluster.len()..];
                continue;
            }
            current.push(Span::styled(head.to_string(), style));
            used += width(head);
            text = &text[head.len()..];
            if used >= limit && !text.is_empty() {
                out.push(Line::from(std::mem::take(&mut current)).style(base));
                used = 0;
            }
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(Line::from(current).style(base));
    }
    out
}



/// Render one buffer line, including any extra lines its annotations require.
///
/// `above` and `below` virtual text become their own screen lines, which is why this returns a
/// vector: one buffer line is not always one screen line.
pub fn render_line(line: &LineRender, theme: &Theme) -> Vec<Line<'static>> {
    render_line_in(line, theme, None)
}

/// As [`render_line`], but knowing how wide the window is.
///
/// The width is only needed for [`VirtTextPos::Right`], which is the one annotation whose position
/// depends on the pane rather than on the text. Passing `None` renders it as if it were `Eol`,
/// which is what a caller with no viewport can honestly do.
pub fn render_line_in(
    line: &LineRender,
    theme: &Theme,
    window_width: Option<usize>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let visible: Vec<_> = line.marks.iter().collect();

    let virt_lines = |pos: VirtTextPos| -> Vec<Line<'static>> {
        visible
            .iter()
            .filter(|m| m.opts.virt_text_pos == pos && !m.opts.virt_text.is_empty())
            .map(|m| {
                Line::from(
                    m.opts
                        .virt_text
                        .iter()
                        .map(|c| {
                            Span::styled(
                                c.text.clone(),
                                c.hl_group.as_deref().map(|g| theme.style(g)).unwrap_or_default(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    };

    out.extend(virt_lines(VirtTextPos::Above));

    // Cut the line at every boundary an annotation introduces, then style each piece by the
    // highest-priority range covering it.
    let text = &line.text;
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for m in &visible {
        let col = (m.col as usize).min(text.len());
        match m.opts.virt_text_pos {
            VirtTextPos::Inline if !m.opts.virt_text.is_empty() => cuts.push(col),
            _ => {}
        }
        if m.opts.hl_group.is_some() {
            cuts.push(col);
            if let Some(e) = m.opts.end_col {
                cuts.push((e as usize).min(text.len()));
            }
        }
    }
    cuts.retain(|c| text.is_char_boundary(*c));
    cuts.sort_unstable();
    cuts.dedup();

    // The row's own background, under everything else on it. Highest priority wins the *base*,
    // and every ranged group is then patched over it — so a syntax colour keeps its foreground and
    // inherits the band, which is the one place in this vocabulary where two marks have to
    // describe the same character at once.
    let band = visible
        .iter()
        .filter(|m| m.opts.line_hl_group.is_some())
        .max_by_key(|m| m.opts.priority);
    let base = band.and_then(|m| m.opts.line_hl_group.as_deref()).map(|g| theme.style(g));

    // A one-shot the whole row shares.
    //
    // The row's band is the only annotation that is *about the row*, so it is the only one that can
    // say "this row just happened" — a flash on a ranged group would light a quarter of a card and
    // leave the rest, which reads as a highlight rather than as an arrival. The moment it counts
    // from is the first time this mark was seen, which is why the mark and not the group carries
    // the timing: an extmark is created once, so this fires once.
    let flash = band.filter(|_| theme.motion()).and_then(|m| {
        let anim = m.opts.line_hl_group.as_deref().and_then(|g| theme.animation(g))?;
        let since = crate::shimmer::first_seen(m.ns, m.id)?;
        crate::shimmer::flash_amount(anim, since)
    });

    let mut spans: Vec<Span<'static>> = Vec::new();
    // Over cut *positions* rather than windows between them, because an empty line has exactly one
    // position and no windows at all — and dropping its inline text there would mean the prompt
    // glyph on an empty composer, the one place it is most needed, is the one place it vanishes.
    for (i, &a) in cuts.iter().enumerate() {
        // Inline virtual text is spliced in at its column, shifting real text right.
        for m in visible.iter().filter(|m| {
            m.opts.virt_text_pos == VirtTextPos::Inline
                && !m.opts.virt_text.is_empty()
                && (m.col as usize).min(text.len()) == a
        }) {
            for c in &m.opts.virt_text {
                spans.push(Span::styled(
                    c.text.clone(),
                    c.hl_group.as_deref().map(|g| theme.style(g)).unwrap_or_default(),
                ));
            }
        }

        let Some(&b) = cuts.get(i + 1) else { continue };
        if a == b {
            continue;
        }
        // Highest priority wins, and on a tie the *narrower* mark does. Two marks at the same
        // priority covering the same character are a general statement and a specific one — a
        // fenced block is code and this word in it is a keyword — and the specific one is the one
        // that was worth saying. Without the tie-break it comes down to which was set last, which
        // is not something a caller should have to know.
        let group = visible
            .iter()
            .filter(|m| m.opts.hl_group.is_some())
            .filter_map(|m| {
                let s = (m.col as usize).min(text.len());
                let e = m.opts.end_col.map(|e| (e as usize).min(text.len())).unwrap_or(s);
                (s <= a && b <= e && e > s).then(|| (m, e - s))
            })
            .max_by_key(|(m, span)| (m.opts.priority, std::cmp::Reverse(*span)))
            .and_then(|(m, _)| m.opts.hl_group.as_deref());
        let style = match (base, group.map(|g| theme.style(g))) {
            (Some(b), Some(g)) => b.patch(g),
            (Some(b), None) => b,
            (None, Some(g)) => g,
            (None, None) => Style::default(),
        };
        // A group that moves becomes one span per grapheme; everything else stays one span, because
        // splitting text that does not need splitting multiplies the span count of every line for
        // no visible difference.
        match group.and_then(|g| theme.animation(g)).filter(|_| theme.motion()) {
            Some(anim) => spans.extend(crate::shimmer::animate(
                &text[a..b],
                style,
                anim,
                theme.truecolor(),
                None,
            )),
            None => spans.push(Span::styled(text[a..b].to_string(), style)),
        }
    }

    // The row's own one-shot, over everything the row says.
    //
    // Applied here rather than inside the loop so that it lands on the *result* of all the other
    // decisions — a diff band, a syntax colour and a sweep are three things that already agreed
    // what a character looks like, and a flash lifts whatever they settled on rather than arguing
    // with them.
    if let Some(t) = flash {
        for span in &mut spans {
            span.style = crate::shimmer::lift(span.style, t, theme.truecolor());
        }
    }

    // Trailing annotations, then right-aligned ones.
    //
    // The order is the grammar: `Eol` means "after the text", `Right` means "at the edge". A row
    // may carry both — a note beside what it says, and a status flush right — so the padding is
    // computed after the trailing chunks are in, not before.
    for m in visible.iter().filter(|m| m.opts.virt_text_pos == VirtTextPos::Eol) {
        if m.opts.virt_text.is_empty() {
            continue;
        }
        spans.push(Span::raw(" "));
        for c in &m.opts.virt_text {
            spans.push(Span::styled(
                c.text.clone(),
                c.hl_group.as_deref().map(|g| theme.style(g)).unwrap_or_default(),
            ));
        }
    }

    let right: Vec<Span<'static>> = visible
        .iter()
        .filter(|m| m.opts.virt_text_pos == VirtTextPos::Right)
        .flat_map(|m| {
            m.opts.virt_text.iter().map(|c| {
                Span::styled(
                    c.text.clone(),
                    c.hl_group.as_deref().map(|g| theme.style(g)).unwrap_or_default(),
                )
            })
        })
        .collect();
    if !right.is_empty() {
        let used: usize = spans.iter().map(|s| width(&s.content)).sum();
        let tail: usize = right.iter().map(|s| width(&s.content)).sum();
        // Only pad when it actually fits. Padding past the edge would push the annotation onto the
        // next row on a wrapping window, which is worse than simply following the text.
        match window_width {
            Some(w) if used + tail < w => spans.push(Span::raw(" ".repeat(w - used - tail))),
            _ if used > 0 => spans.push(Span::raw(" ")),
            _ => {}
        }
        spans.extend(right);
    }

    out.push(match base {
        Some(b) => Line::from(spans).style(b),
        None => Line::from(spans),
    });
    out.extend(virt_lines(VirtTextPos::Below));
    out
}

/// Run a row's own background out to the right edge of the window.
///
/// `Paragraph` styles the cells a line's spans occupy and leaves the rest of the row at whatever the
/// terminal already was, which for a diff means a green band that stops where the code stops. The
/// band is the thing that says "this line changed", and one that ends in a ragged edge halfway
/// across says it about the first forty columns.
///
/// Padding here rather than where the row was written is what makes it survive a resize: the width
/// is the window's, now, and these rows are rebuilt from the mirror on every draw.
fn fill_to_edge(line: Line<'static>, cols: usize) -> Line<'static> {
    if line.style == Style::default() {
        return line;
    }
    let used: usize = line.spans.iter().map(|s| width(&s.content)).sum();
    if used >= cols {
        return line;
    }
    let pad = Span::raw(" ".repeat(cols - used));
    let mut spans = line.spans;
    spans.push(pad);
    Line::from(spans).style(line.style)
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Turn a size request into cells, border included.
///
/// `content` is what the buffer needs, `chrome` the columns or rows the border will eat, and
/// `available` the whole axis. Every variant sizes *content* — see [`Extent`] — so the chrome is
/// added on afterwards and the sum is clamped to what there is.
///
/// The clamp is why the border has to be part of this function rather than added by the caller: a
/// float asking for more than fits must lose content rows, not draw its bottom edge off-screen.
fn resolve_extent(e: Extent, content: u16, chrome: u16, available: u16) -> u16 {
    let v = match e {
        Extent::Fixed { n } => n.saturating_add(chrome),
        Extent::Auto => content.saturating_add(chrome),
        Extent::Max { n } => content.min(n).saturating_add(chrome),
        Extent::Fill => available,
    };
    v.clamp(1, available.max(1))
}

/// Turn declarative geometry into rectangles.
///
/// Docks are carved off the edges in a fixed order; whatever is left is `Main`. Floats are placed
/// afterwards against their anchor, and clamped so a float can never be positioned off-screen —
/// a plugin computing an offset from a stale viewport would otherwise render nothing at all.
/// Columns of border drawn between a side dock and what is beside it.
///
/// One, and only for side docks. Text running straight from a panel into a transcript with no gap
/// reads as one column of nonsense — the eye has nothing to stop at. A bottom dock needs no rule
/// because a line break already separates it.
const DOCK_RULE: u16 = 1;

/// A line drawn between two regions, and which way it runs.
///
/// Orientation is carried rather than inferred from the rectangle's shape. A one-cell-square rule —
/// two panes meeting inside a region three cells across — is a legitimate degenerate case, and
/// "width is 1, so it must be vertical" answers it wrong half the time. It is one glyph, so getting
/// it wrong is not a catastrophe; it is also one field, so there is no reason to guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub rect: Rect,
    pub vertical: bool,
    /// Whether this line is an edge of the pane that has the keyboard.
    ///
    /// Which pane you are typing into has to be visible without hunting for the caret. Four panes
    /// of one conversation look identical, and the caret is one cell — so the thing that says
    /// *here* is the border, lit on the sides that belong to the active pane. It is what tmux does,
    /// it costs no rows, and unlike a title bar per pane it costs nothing at all when there is only
    /// one pane, because then there are no borders.
    pub active: bool,
}

pub fn resolve_layout(mirror: &Mirror, area: Rect) -> Vec<(WindowId, Rect)> {
    resolve_layout_with_rules(mirror, area).0
}

/// Layout, plus where the separators go.
///
/// Three passes, in this order and for this reason:
///
/// 1. **Windows docked to the screen** — the sidebar, the tab bar, the status line — are carved off
///    the edges. What is left is the main region.
/// 2. **The active tab's pane tree** divides that region into one rectangle per pane, with a rule
///    between every two.
/// 3. **Windows docked in a pane** are carved off *its* rectangle by the same code that did step 1,
///    which is the whole reason a pane needed no geometry vocabulary of its own.
///
/// Then floats, against whatever they are anchored to.
///
/// A window naming a pane that is not in the active tab gets no rectangle and is not drawn. That is
/// how tabs work at all: switching tabs moves no windows and rebuilds nothing, it changes which
/// pane ids have rectangles this frame.
pub fn resolve_layout_with_rules(
    mirror: &Mirror,
    area: Rect,
) -> (Vec<(WindowId, Rect)>, Vec<Rule>) {
    let mut out = Vec::new();
    let mut rules: Vec<Rule> = Vec::new();

    // Windows measured against the screen rather than against a pane — and *not* the one asking
    // for the main region, which is settled below once the panes are known. Carved here as well it
    // would be placed twice: once filling the whole region, and once in the active pane, with the
    // first of the two winning every lookup that takes the earliest match.
    let screen: Vec<WindowId> = mirror
        .window_order
        .iter()
        .copied()
        .filter(|w| {
            matches!(mirror.windows.get(w).map(|w| &w.layout),
                Some(WindowLayout::Docked { pane: None, dock, .. }) if *dock != Dock::Main)
        })
        .collect();
    let main = carve(mirror, area, &screen, &mut out, &mut rules, false);

    // The active tab's panes, and what is docked inside each.
    let tree = mirror
        .active_tab
        .and_then(|id| mirror.tabs.iter().find(|t| t.id == id))
        .map(|t| &t.root);

    let mut pane_mains: Vec<(neosh_proto::PaneId, Rect)> = Vec::new();
    if let Some(root) = tree {
        let mut panes = Vec::new();
        let active = mirror
            .active_tab
            .and_then(|id| mirror.tabs.iter().find(|t| t.id == id))
            .map(|t| t.active_pane);
        divide(root, main, active, &mut panes, &mut rules);
        for (pane, rect) in panes {
            let inside: Vec<WindowId> = mirror
                .window_order
                .iter()
                .copied()
                .filter(|w| {
                    matches!(mirror.windows.get(w).map(|w| &w.layout),
                        Some(WindowLayout::Docked { pane: Some(p), .. }) if *p == pane)
                })
                .collect();
            let leftover = carve(mirror, rect, &inside, &mut out, &mut rules, true);
            pane_mains.push((pane, leftover));
        }
    }

    // `Dock::Main` on the *screen* means the main region, which — once there are panes — is the one
    // you are looking at. A plugin opening a window without naming a pane is the ordinary case, and
    // it should land where the person is, exactly as a float that names no view does. Only if the
    // active pane has nothing of its own in that slot: a pane that has been furnished with a
    // transcript already answers "what is in the main region here", and two windows in one `Main`
    // slot is the one thing that dock promises never happens.
    let active_pane = mirror
        .active_tab
        .and_then(|id| mirror.tabs.iter().find(|t| t.id == id))
        .map(|t| t.active_pane);
    let stray = mirror
        .window_order
        .iter()
        .copied()
        .find(|w| matches!(mirror.windows.get(w).map(|w| &w.layout),
            Some(WindowLayout::Docked { pane: None, dock: Dock::Main, .. })));
    if let Some(win) = stray {
        let slot = match active_pane.and_then(|p| pane_mains.iter().find(|(id, _)| *id == p)) {
            // The active pane has a `Main` window already: that slot is taken, and this one is not
            // drawn rather than drawn over it.
            Some((_, rect)) if out.iter().any(|(_, r)| r == rect) => None,
            Some((_, rect)) => Some(*rect),
            // No panes at all — a frontend that has not been told about them, a test driving the
            // mirror directly. The main region is the whole of what the docks left.
            None if tree.is_none() => Some(main),
            None => None,
        };
        if let Some(rect) = slot {
            out.push((win, rect));
        }
    }

    // What the pane-relative anchors are measured against: the region the person is looking at.
    let anchor_main = active_pane
        .and_then(|p| pane_mains.iter().find(|(id, _)| *id == p))
        .map(|(_, r)| *r)
        .unwrap_or(main);

    resolve_floats(mirror, area, anchor_main, &mut out);
    (out, rules)
}

/// Carve every window in `windows` off the edges of `rect`, and answer with what is left.
///
/// `inside_pane` says whether this is a pane's rectangle, which changes exactly one thing: a
/// side dock inside a pane draws no rule, because the pane already has one on that side and two
/// adjacent separators read as a box somebody forgot to finish.
fn carve(
    mirror: &Mirror,
    rect: Rect,
    windows: &[WindowId],
    out: &mut Vec<(WindowId, Rect)>,
    rules: &mut Vec<Rule>,
    inside_pane: bool,
) -> Rect {
    let mut main = rect;

    let dock_of = |w: &WindowId| match mirror.windows.get(w).map(|w| &w.layout) {
        Some(WindowLayout::Docked { dock, size, .. }) => Some((*dock, *size)),
        _ => None,
    };

    for dock in [Dock::Left, Dock::Right, Dock::Top, Dock::Bottom] {
        // Every window on this edge, not just the first. Stacking them is what lets a status line
        // sit under the composer, and what stops a plugin's panel from being silently invisible
        // because something was already docked there.
        let docked: Vec<WindowId> = windows
            .iter()
            .copied()
            .filter(|w| dock_of(w).map(|d| d.0) == Some(dock))
            .collect();

        for win in docked {
            let size = dock_of(&win).and_then(|d| d.1);
            match dock {
                Dock::Left | Dock::Right => {
                    // At least one column has to survive for the main area, so a dock plus its rule
                    // get at most `width - 1`. When even that is zero there is no room to split at
                    // all, and the window is dropped rather than drawn over the only column there
                    // is. The rule is the first thing given up when space runs short: a separator
                    // that costs you the panel it separates is not a good trade.
                    let avail = main.width.saturating_sub(1);
                    if avail == 0 {
                        continue;
                    }
                    let w = size.unwrap_or(main.width / 4).clamp(1, avail);
                    // A pane already has a rule down the side it shares with its neighbour, and a
                    // second one a column away reads as an unfinished box rather than as two
                    // separations.
                    let rule = if avail > w && !inside_pane { DOCK_RULE } else { 0 };
                    let (a, b) = if dock == Dock::Left {
                        if rule > 0 {
                            rules.push(Rule {
                                rect: Rect { x: main.x + w, width: 1, ..main },
                                vertical: true,
                                active: false,
                            });
                        }
                        (Rect { width: w, ..main }, Rect {
                            x: main.x + w + rule,
                            width: main.width - w - rule,
                            ..main
                        })
                    } else {
                        let edge = main.x + main.width - w;
                        if rule > 0 {
                            rules.push(Rule {
                                rect: Rect { x: edge - 1, width: 1, ..main },
                                vertical: true,
                                active: false,
                            });
                        }
                        (
                            Rect { x: edge, width: w, ..main },
                            Rect { width: main.width - w - rule, ..main },
                        )
                    };
                    out.push((win, a));
                    main = b;
                }
                Dock::Top => {
                    let avail = main.height.saturating_sub(1);
                    if avail == 0 {
                        continue;
                    }
                    let h = size.unwrap_or(1).clamp(1, avail);
                    out.push((win, Rect { height: h, ..main }));
                    main = Rect { y: main.y + h, height: main.height - h, ..main };
                }
                Dock::Bottom => {
                    let avail = main.height.saturating_sub(1);
                    if avail == 0 {
                        continue;
                    }
                    let base = size.unwrap_or(main.height / 4);
                    // A wrapping bottom dock grows to fit what wrapped, its declared size becoming
                    // a floor: folding a long prompt and then hiding the folded rows would be
                    // clipping with extra steps. Capped at half of what is left, because the
                    // transcript above is the thing the prompt is answering.
                    //
                    // Counted with a bare theme rather than the live one, so layout does not need
                    // the styling pipeline threaded through it: colours cannot change how many
                    // rows a line folds into — only the text and the window's width can.
                    let h = match mirror.windows.get(&win) {
                        Some(w) if w.wraps() => {
                            let rows = rendered_rows(
                                mirror,
                                w,
                                &Theme::new(crate::theme::ColorDepth::Ansi16),
                                main.width,
                            )
                            .len() as u16;
                            rows.clamp(base, (avail / 2).max(base))
                        }
                        _ => base,
                    };
                    let h = h.clamp(1, avail);
                    out.push((win, Rect { y: main.y + main.height - h, height: h, ..main }));
                    main = Rect { height: main.height - h, ..main };
                }
                Dock::Main => {}
            }
        }
    }

    // Exactly one, per rectangle. A second window asking for the same slot is not drawn rather than
    // drawn over the first: `Dock::Main` is the one dock that promises to be alone in its region,
    // and two panels stacked pixel-for-pixel is a bug you diagnose by closing things one at a time.
    if let Some(win) = windows.iter().find(|w| dock_of(w).map(|d| d.0) == Some(Dock::Main)) {
        if main.width > 0 && main.height > 0 {
            out.push((*win, main));
        }
    }

    main
}

/// Turn a pane tree into one rectangle per pane, with a rule between every two siblings.
///
/// Space is divided by weight with the remainder handed out largest-first, so the rectangles sum to
/// exactly the region rather than leaving a column of nothing down one edge. When there is not
/// enough room for every pane to have a cell, the separators go first — chrome before content, as
/// with [`DOCK_RULE`] — and then the panes that do not fit get no rectangle at all and are simply
/// not drawn. Which is the honest answer: a pane rendered zero cells wide is a pane whose contents
/// have silently vanished, and one with no rectangle is a pane you can still `<C-w>h` back out of.
fn divide(
    node: &neosh_proto::PaneNode,
    rect: Rect,
    active: Option<neosh_proto::PaneId>,
    out: &mut Vec<(neosh_proto::PaneId, Rect)>,
    rules: &mut Vec<Rule>,
) {
    use neosh_proto::{PaneNode, SplitDir};

    // A rectangle with no cells in it is not a placement. Nothing downstream draws one, but a pane
    // that "has" a rectangle is a pane windows get docked into, and a docked window is asked how
    // many rows its content folds into — so the empty case is refused here rather than caught by
    // each of the four places that would otherwise have to.
    if rect.width == 0 || rect.height == 0 {
        return;
    }

    let (dir, children) = match node {
        PaneNode::Leaf { pane } => {
            out.push((*pane, rect));
            return;
        }
        PaneNode::Split { dir, children } => (*dir, children),
    };
    if children.is_empty() {
        return;
    }

    let vertical = dir == SplitDir::Row;
    let axis = if vertical { rect.width } else { rect.height };
    let n = children.len() as u16;

    // One cell between each pair, given up entirely before any pane is starved of its own.
    let seps = if axis > n { n.saturating_sub(1) } else { 0 };
    let avail = axis.saturating_sub(seps);
    let sizes = share(avail, &children.iter().map(|c| c.weight).collect::<Vec<_>>());

    let mut at = if vertical { rect.x } else { rect.y };
    for (i, (child, size)) in children.iter().zip(sizes.iter().copied()).enumerate() {
        if size == 0 {
            continue;
        }
        let piece = if vertical {
            Rect { x: at, width: size, ..rect }
        } else {
            Rect { y: at, height: size, ..rect }
        };
        divide(&child.node, piece, active, out, rules);
        at += size;

        // A rule after every child but the last one that got space. Drawn between this pane and
        // whatever comes next, which is why it is placed here rather than derived afterwards from
        // two rectangles: at small sizes some children have none, and "between the rectangles" then
        // means a line beside a pane that is not there.
        let next = children[i + 1..].iter().zip(sizes[i + 1..].iter()).find(|(_, s)| **s > 0);
        if seps > 0 && let Some((after, _)) = next {
            // Lit when the pane with the keyboard is on either side of it. Both sides, because a
            // border belongs to the two panes it divides and lighting only the near one would draw
            // half a box around the active pane and half around its neighbour.
            let touches = |n: &neosh_proto::PaneNode| active.is_some_and(|a| n.contains(a));
            let lit = touches(&child.node) || touches(&after.node);
            let r = if vertical {
                Rule { rect: Rect { x: at, width: 1, ..rect }, vertical: true, active: lit }
            } else {
                Rule { rect: Rect { y: at, height: 1, ..rect }, vertical: false, active: lit }
            };
            rules.push(r);
            at += 1;
        }
    }
}

/// Divide `total` cells among `weights`, summing to exactly `total`.
///
/// Largest-remainder: floor everything, then hand the leftover out to whoever was rounded down
/// hardest. Plain rounding loses up to one cell per pane, and a column of unpainted terminal down
/// the right-hand edge of a four-way split is the kind of thing that looks like a rendering bug
/// forever because nothing in the layout is actually wrong.
fn share(total: u16, weights: &[u16]) -> Vec<u16> {
    let sum: u32 = weights.iter().map(|w| u32::from(*w)).sum();
    if weights.is_empty() || sum == 0 {
        return vec![0; weights.len()];
    }
    let mut sizes: Vec<u16> = Vec::with_capacity(weights.len());
    let mut remainders: Vec<(u32, usize)> = Vec::with_capacity(weights.len());
    let mut given = 0u32;
    for (i, w) in weights.iter().enumerate() {
        let exact = u32::from(total) * u32::from(*w);
        let whole = exact / sum;
        sizes.push(whole as u16);
        remainders.push((exact % sum, i));
        given += whole;
    }
    // Biggest remainder first, and by index where two are equal — so the same layout at the same
    // size always divides the same way, rather than shifting a column when a `HashMap` reorders.
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut left = u32::from(total).saturating_sub(given);
    for (_, i) in remainders {
        if left == 0 {
            break;
        }
        sizes[i] += 1;
        left -= 1;
    }
    sizes
}

/// Place every float against whatever it is anchored to.
///
/// `main` is the region the dock anchors are measured from — which, once panes exist, is the
/// *active pane's* main slot rather than the whole region. A completion menu says "against the
/// message field" and nothing else, and with two chats side by side there are two message fields;
/// the one being typed into is the one the menu belongs over.
fn resolve_floats(mirror: &Mirror, area: Rect, main: Rect, out: &mut Vec<(WindowId, Rect)>) {
    // Floats, in paint order so later entries draw on top.
    for win in mirror.paint_order() {
        let Some(w) = mirror.windows.get(&win) else { continue };
        let WindowLayout::Float { config } = &w.layout else { continue };

        let lines = mirror.buffers.get(&w.buf).map(|b| b.lines.len()).unwrap_or(0) as u16;
        let longest = mirror
            .buffers
            .get(&w.buf)
            .map(|b| b.lines.iter().map(|l| width(&l.text)).max().unwrap_or(0))
            .unwrap_or(0) as u16;

        let chrome = u16::from(config.border != neosh_proto::BorderStyle::None) * 2;
        let cw = resolve_extent(config.width, longest, chrome, area.width);
        let ch = resolve_extent(config.height, lines, chrome, area.height);

        let (base_x, base_y) = match config.anchor {
            Anchor::Screen => (
                area.x + area.width.saturating_sub(cw) / 2,
                area.y + area.height.saturating_sub(ch) / 2,
            ),
            Anchor::Cursor => {
                // Anchored to the focused window's cursor, which is the only place the core needs
                // realized geometry back from the frontend.
                let anchor_win = mirror.focus.and_then(|f| mirror.windows.get(&f));
                let (row, col) = anchor_win.map(|w| w.cursor).unwrap_or((0, 0));
                let host = out
                    .iter()
                    .find(|(id, _)| Some(*id) == mirror.focus)
                    .map(|(_, r)| *r)
                    .unwrap_or(area);
                (host.x + col.min(u16::MAX as u32) as u16, host.y + row.min(u16::MAX as u32) as u16 + 1)
            }
            Anchor::Window { win: target } => {
                out.iter().find(|(id, _)| *id == target).map(|(_, r)| (r.x, r.y)).unwrap_or((area.x, area.y))
            }
            // Flush against the dock, on the main region's side of it — the float's near edge
            // touching the dock's, not its far corner landing on it.
            //
            // Which is the whole point: a completion is placed by saying "against the message
            // field" and nothing else. Anchoring the float's *top-left* to the strip would make
            // every caller subtract its own height first, and the day one of them subtracts a
            // number that is one out, the menu sits over the field it is completing. `main` is
            // what the docks left behind, so each of these is the edge that dock was carved from.
            Anchor::Dock { dock } => match dock {
                Dock::Bottom => (main.x, (main.y + main.height).saturating_sub(ch)),
                Dock::Top => (main.x, main.y),
                Dock::Left => (main.x, main.y),
                Dock::Right => ((main.x + main.width).saturating_sub(cw), main.y),
                Dock::Main => (main.x, main.y),
            },
        };

        let x = (base_x as i64 + config.offset.col as i64)
            .clamp(area.x as i64, (area.x + area.width).saturating_sub(cw) as i64) as u16;
        let y = (base_y as i64 + config.offset.row as i64)
            .clamp(area.y as i64, (area.y + area.height).saturating_sub(ch) as i64) as u16;

        out.push((win, Rect { x, y, width: cw, height: ch }));
    }
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Draw one frame, and report where the caret belongs.
///
/// The caret is not decoration. Without one the composer gives no sign that typing goes anywhere,
/// and a terminal program with no visible insertion point reads as frozen.
pub fn draw(frame: &mut Frame, mirror: &Mirror, theme: &Theme) -> Drawn {
    let area = frame.area();
    let (rects, rules) = resolve_layout_with_rules(mirror, area);
    // Which window the caret belongs to is decided before anything is drawn, because the window it
    // belongs to is also the one whose scroll has to bend to keep it on screen.
    let caret_win = caret_target(mirror, &rects);
    let mut caret = None;
    let mut tops: Vec<(WindowId, (u32, u32))> = Vec::new();

    // Drawn before the windows so a float still covers it, and before content so nothing has to
    // know the rule is there.
    let rule_style = theme.style("Separator");
    // The pane you are typing into, said with its own edges. Drawn *after* the dim ones would be
    // simpler, but the order below is verticals-then-horizontals for the junction lookup — so the
    // style is chosen per rule instead.
    let lit_style = theme.style("Pane.Active");
    let style_of = |r: &Rule| if r.active { lit_style } else { rule_style };
    // Verticals first, then horizontals, so that where the two meet the horizontal is drawn over
    // the vertical and the junction can be read off what is already in the cell. Both orders draw
    // the same crossing; doing it in one fixed order is what makes the junction lookup a lookup
    // rather than a guess about which line got there first.
    for r in rules.iter().filter(|r| r.vertical) {
        for y in r.rect.y..r.rect.y.saturating_add(r.rect.height) {
            if let Some(cell) = frame.buffer_mut().cell_mut((r.rect.x, y)) {
                cell.set_symbol("│");
                cell.set_style(style_of(r));
            }
        }
    }
    for r in rules.iter().filter(|r| !r.vertical) {
        for x in r.rect.x..r.rect.x.saturating_add(r.rect.width) {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, r.rect.y)) {
                // A horizontal meeting a vertical is a tee, not a crossed-out line. Which way the
                // tee points depends on how far the vertical runs past this row, and that is not
                // knowable from one cell — so the four-way piece is used wherever they meet. It is
                // very slightly wrong at a T-junction and unmistakably right at a cross, where the
                // alternative reads as two panes that are not actually divided.
                let symbol = if cell.symbol() == "│" { "┼" } else { "─" };
                cell.set_symbol(symbol);
                // A crossing where one of the two lines is lit stays lit: the junction belongs to
                // the active pane's outline, and dimming it would break the outline at its corners
                // — which are the parts of a box the eye actually uses.
                if r.active || cell.style() != lit_style {
                    cell.set_style(style_of(r));
                }
            }
        }
    }
    let order = mirror.paint_order();

    for win in order {
        let Some((_, rect)) = rects.iter().find(|(id, _)| *id == win) else { continue };
        let Some(w) = mirror.windows.get(&win) else { continue };
        let rect = *rect;
        if rect.width == 0 || rect.height == 0 {
            continue;
        }
        // A window somebody restyled reads every group name through its own map — border, title,
        // `Normal`, every mark — and the rest of the screen never knows.
        let local;
        let theme = if w.highlights.is_empty() {
            theme
        } else {
            local = theme.remapped(&w.highlights);
            &local
        };

        let (inner, block) = match &w.layout {
            WindowLayout::Float { config } if config.border != neosh_proto::BorderStyle::None => {
                let mut b = Block::default().borders(Borders::ALL).border_type(
                    match config.border {
                        neosh_proto::BorderStyle::Double => BorderType::Double,
                        neosh_proto::BorderStyle::Thick => BorderType::Thick,
                        neosh_proto::BorderStyle::Single => BorderType::Plain,
                        _ => BorderType::Rounded,
                    },
                );
                b = b.border_style(border_style(
                    theme,
                    config.border_hl.as_deref().unwrap_or("Float.Border"),
                ));
                if let Some(t) = &config.title {
                    b = b.title(Span::styled(t.clone(), theme.style("Float.Title")));
                }
                (b.inner(rect), Some(b))
            }
            _ => (rect, None),
        };

        // Floats are opaque: clear whatever is underneath, or the dock behind shows through.
        if w.layout_is_float() {
            frame.render_widget(ratatui::widgets::Clear, rect);
        }
        if let Some(b) = block {
            frame.render_widget(b, rect);
        }

        // Wrapping is per window: the chat has to wrap or a long answer is silently cut off at the
        // right edge, while the sidebar and the status line must clip or one long path would push
        // everything below it off the screen.
        let rendered = rendered_rows(mirror, w, theme, inner.width);
        let here = (caret_win == Some(win)).then(|| caret_in(mirror, w, &rendered, inner.width)).flatten();

        // Follow the tail. Taking the *first* n lines shows a long conversation's opening and
        // never its answer, which is the wrong end of every transcript ever written. An explicit
        // `top_line` still means "start here", so paging keeps working — and `Some(0)` is one of
        // those, not this. Read as "unscrolled" it was impossible to scroll to the first row of a
        // transcript: asking put the window back at the last one.
        let height = inner.height as usize;
        let unscrolled = w.follows_tail() && w.top_line.is_none();
        let mut start = match rendered.len().checked_sub(height) {
            Some(over) if unscrolled && over > 0 => over,
            _ => 0,
        };
        // And then: the cursor is never off the window it is in. `top_line` counts buffer rows,
        // one of which is several screen rows the moment it wraps, so a scroll that put the cursor
        // on screen by that arithmetic can still leave it well below the bottom edge — which is
        // how the transcript reader opened with no cursor anywhere on it. Minimally, the way a
        // scroll that chases a cursor has to be: any more and reading downwards would jump.
        if let Some((idx, _)) = here {
            if idx < start {
                start = idx;
            } else if height > 0 && idx >= start + height {
                start = idx + 1 - height;
            }
        }
        let start = start.min(rendered.len());
        // What is actually on screen, in buffer rows — which is not what the core asked for once
        // the caret has bent it, and is not `height` either once anything wraps. Both are numbers
        // only the frontend can produce, and everything that pages by a screenful reads them.
        let shown = &rendered[start..rendered.len().min(start + height.max(1))];
        let mut seen: Option<(u32, u32)> = None;
        for (row, _) in shown.iter().filter_map(|r| r.at) {
            seen = Some(match seen {
                Some((first, _)) => (first, row),
                None => (row, row),
            });
        }
        tops.push((
            win,
            seen.map(|(first, last)| (first, last + 1 - first))
                .unwrap_or((w.top_line.unwrap_or(0), 0)),
        ));
        let lines: Vec<Line> =
            rendered[start..].iter().take(height).map(|r| r.line.clone()).collect();

        // Gravity only bites when the content does not fill the window — the case a scroll offset
        // cannot express, because there is nothing to scroll. Done by shrinking the rectangle
        // rather than by padding with blank rows, so nothing downstream has to know the difference
        // between a line the buffer has and a line put there to take up room.
        let inner = match (&w.layout, height.checked_sub(lines.len())) {
            (
                WindowLayout::Docked { gravity: neosh_proto::Gravity::End, .. },
                Some(slack),
            ) if slack > 0 => Rect {
                y: inner.y.saturating_add(slack as u16),
                height: inner.height.saturating_sub(slack as u16),
                ..inner
            },
            _ => inner,
        };

        // Gravity has already moved `inner` down by however many rows the content falls short of,
        // so the caret moves with it by construction rather than by a second sum that has to agree.
        if let Some((idx, column)) = here {
            let line = u16::try_from(idx.saturating_sub(start)).unwrap_or(u16::MAX);
            if line < inner.height && column < inner.width {
                caret = Some((inner.x + column, inner.y + line));
            }
        }

        frame.render_widget(Paragraph::new(lines).style(theme.style("Normal")), inner);

        // A surface belongs to its window, so it is painted with it — *inside* the paint order
        // rather than after it. Drawn in one pass at the end, every surface was on top of
        // everything: a terminal pane filling a rectangle covered any float over it, so holding the
        // window prefix in a shell drew the panel listing what follows and then painted the shell
        // straight back over it. The one thing on screen that has to be visible over a full-screen
        // program is the way out of it.
        for surface in mirror.surfaces.values().filter(|s| s.win == win) {
            for cell in &surface.cells {
                let x = rect.x + surface.rect.col + cell.col;
                let y = rect.y + surface.rect.row + cell.row;
                if x >= rect.x + rect.width || y >= rect.y + rect.height {
                    continue;
                }
                if let Some(target) = frame.buffer_mut().cell_mut((x, y)) {
                    target.set_symbol(&cell.grapheme);
                    if let Some(fg) = cell.fg {
                        target.set_fg(theme.color(fg));
                    }
                    if let Some(bg) = cell.bg {
                        target.set_bg(theme.color(bg));
                    }
                    target.set_style(
                        Style::default().add_modifier(crate::theme::modifiers(&cell.attrs)),
                    );
                }
            }
        }
    }

    draw_notifications(frame, area, mirror, theme);

    // The block cursor, last of all and over whatever the row under it ended up being.
    //
    // Painted as a cell rather than left to the terminal's own caret, because the terminal's is a
    // thin bar in most of them and the one thing it has to do here is be findable in a screenful
    // of prose. The terminal's caret is still put in the same place — it is what a screen reader
    // and a terminal's own "where am I" follow — this just makes it visible.
    if let Some(((x, y), true)) = caret.zip(
        caret_win
            .and_then(|id| mirror.windows.get(&id))
            .map(|w| w.cursor_shape == CursorShape::Block),
    ) && let Some(cell) = frame.buffer_mut().cell_mut((x, y))
    {
        // Reverse video when nothing has said otherwise. A palette entry is how you change what a
        // block cursor looks like, never whether there is one — and a theme that has never heard
        // of `Cursor` giving you a mode with no visible cursor in it is the whole bug.
        let style = if theme.defines("Cursor") {
            theme.style("Cursor")
        } else {
            Style::default().add_modifier(ratatui::style::Modifier::REVERSED)
        };
        cell.set_style(cell.style().patch(style));
    }

    Drawn { caret, caret_win: caret.and(caret_win), tops }
}

/// What one frame turned out to be: where the caret goes, and what each window actually showed.
///
/// The tops are not what the core asked for. A window whose scroll was bent to keep its cursor on
/// screen is showing a different first row than `top_line` says, and every plugin that pages by a
/// screenful reads that number.
#[derive(Debug, Clone, Default)]
pub struct Drawn {
    pub caret: Option<(u16, u16)>,
    /// The window the caret is in, whose cursor shape decides what it looks like.
    pub caret_win: Option<WindowId>,
    /// Per window: the first buffer row drawn, and how many buffer rows were drawn.
    pub tops: Vec<(WindowId, (u32, u32))>,
}

/// Every screen line a window's visible buffer range turns into, marks and wrapping applied.
///
/// Shared by drawing and by the caret, which is the point: gravity moves content down by however
/// many rows it falls short, and the caret has to move by the *same* number. Two calculations of
/// "how many lines is this" would agree until the first wrapped line, and then quietly stop.
/// A float's border style, with whatever motion its group carries applied to the frame as a whole.
///
/// A `Block` draws its own glyphs and takes one style for all of them, so a border cannot carry a
/// travelling gradient the way a run of text can. What it *can* carry is the colour that gradient
/// is currently passing through: animate one cluster, keep the style it came back with, and the
/// whole frame drifts together. Which is the better reading anyway — a rainbow chasing itself
/// round a border is something you end up watching, and a border is the edge of the thing you are
/// meant to be looking at.
///
/// Gated on `ui.motion` like every other animation here, and `Animation::Flash` never fires: it
/// counts from the moment its *mark* first appeared and a border has no mark.
fn border_style(theme: &Theme, name: &str) -> ratatui::style::Style {
    let base = theme.style(name);
    match theme.animation(name).filter(|_| theme.motion()) {
        Some(anim) => crate::shimmer::animate("─", base, anim, theme.truecolor(), None)
            .first()
            .map_or(base, |span| span.style),
        None => base,
    }
}

/// One screen row: what to draw, and which buffer row it came from.
///
/// The provenance is the whole point. A caret that works its own screen row out from a buffer row
/// counts one thing while the draw counts another, and the two agree exactly until the first line
/// long enough to wrap — after which the caret is placed above the character it is on, and far
/// enough into a transcript of paragraphs it is off the window altogether and therefore hidden.
/// One calculation, read by both.
struct ScreenRow {
    line: Line<'static>,
    /// The buffer row this is part of and which wrapped segment of it, or `None` for a virtual
    /// line — a row that exists on the screen and not in the text.
    at: Option<(u32, u16)>,
}

fn rendered_rows(
    mirror: &Mirror,
    w: &crate::mirror::MirrorWindow,
    theme: &Theme,
    width: u16,
) -> Vec<ScreenRow> {
    // Wrapping is per window: the chat has to wrap or a long answer is silently cut off at the
    // right edge, while the sidebar and the status line must clip or one long path would push
    // everything below it off the screen.
    let wraps = w.wraps();
    let cols = (width as usize).max(1);
    let Some(b) = mirror.buffers.get(&w.buf) else { return Vec::new() };
    let mut out: Vec<ScreenRow> = Vec::new();
    for (row, l) in b.lines.iter().enumerate().skip(w.top_line.unwrap_or(0) as usize) {
        // `render_line_in` emits the virtual lines that go above, then the row's own text, then
        // the ones that go below. Which of them is the text is exactly what the caret needs.
        let above = l
            .marks
            .iter()
            .filter(|m| m.opts.virt_text_pos == VirtTextPos::Above && !m.opts.virt_text.is_empty())
            .count();
        for (i, line) in render_line_in(l, theme, Some(cols)).into_iter().enumerate() {
            let pieces = if wraps { wrap_line(&line, cols) } else { vec![line] };
            for (seg, piece) in pieces.into_iter().enumerate() {
                out.push(ScreenRow {
                    line: fill_to_edge(piece, cols),
                    at: (i == above).then_some((row as u32, seg as u16)),
                });
            }
        }
    }
    out
}

/// Where the caret goes, in screen cells.
///
/// Which window the caret belongs to.
///
/// The focused window if there is one, else the bottom-most docked window — which is the composer,
/// and is where typing lands when nothing else has claimed the keyboard.
fn caret_target(mirror: &Mirror, rects: &[(WindowId, Rect)]) -> Option<WindowId> {
    mirror.focus.or(mirror.home).or_else(|| {
        // Neither a float holding focus nor a core that has said where the keyboard rests: a
        // frontend talking to something older, or a test driving the mirror by hand. The composer,
        // guessed as the bottom-most docked window — which is exact for a terminal with one of
        // them, and is the reason `HomeChanged` had to exist for terminals with two.
        rects
            .iter()
            .filter(|(id, _)| {
                mirror
                    .windows
                    .get(id)
                    .is_some_and(|w| matches!(w.layout, WindowLayout::Docked { dock: Dock::Bottom, .. }))
            })
            .min_by_key(|(_, r)| r.y)
            .map(|(id, _)| *id)
    })
}

/// Where a window's cursor sits in the rows that window rendered to: which one, and how far along.
///
/// Both numbers are read off the *rendered* rows rather than computed a second time from the
/// buffer, which is what makes the caret and the text it is on inseparable. `None` when the cursor
/// is on a row this window did not render — above its top, or past the end of the buffer.
fn caret_in(
    mirror: &Mirror,
    w: &crate::mirror::MirrorWindow,
    rendered: &[ScreenRow],
    width: u16,
) -> Option<(usize, u16)> {
    let (row, col) = w.cursor;
    let buffer = mirror.buffers.get(&w.buf)?;
    let line_render = buffer.lines.get(row as usize);
    let text = line_render.map(|l| l.text.as_str()).unwrap_or("");
    // Byte offset to screen column happens here, and only here — the same conversion every mark
    // goes through, so a cursor after an emoji lands where the emoji ends.
    let mut column = display_col(text, col as usize) as u16;
    // Virtual text is drawn but does not exist in the buffer, so the caret has to be pushed past
    // whatever was spliced in ahead of it. Without this, chrome around a text field — a prompt
    // glyph, a rule above it — silently moves the caret away from the character it is on, and the
    // bug only shows up as "typing appears one column off", which is a nightmare to trace back.
    column = column.saturating_add(inline_before(line_render, col as usize));

    // On a wrapped line the caret moves down as well as along, onto the segment it fell into.
    let cols = width.max(1);
    let (seg, column) = if w.wraps() { (column / cols, column % cols) } else { (0, column) };
    let idx = rendered.iter().position(|r| r.at == Some((row, seg)))?;
    Some((idx, column))
}

/// How many screen columns of inline virtual text sit before `col` on this row.
///
/// Inline chunks are spliced in at their own column, so everything at or before the caret's byte
/// offset displaces it. At-or-before rather than strictly-before: a prompt at column 0 has to push
/// a caret that is also at column 0, which is where the caret spends most of its life in an empty
/// composer.
fn inline_before(line: Option<&LineRender>, col: usize) -> u16 {
    let Some(line) = line else { return 0 };
    let shift: usize = line
        .marks
        .iter()
        .filter(|m| m.opts.virt_text_pos == VirtTextPos::Inline)
        .filter(|m| (m.col as usize) <= col)
        .flat_map(|m| m.opts.virt_text.iter())
        .map(|c| width(&c.text))
        .sum();
    u16::try_from(shift).unwrap_or(u16::MAX)
}

/// How long a message stays on screen.
///
/// There is no frame loop, so nothing repaints purely to expire one: a message outlives its welcome
/// until the next real mutation. That is the honest trade for having no timer — and in a session
/// where anything at all is happening, the next frame is milliseconds away.
const NOTIFICATION_TTL: std::time::Duration = std::time::Duration::from_secs(6);

/// How long news stays, which is longer than a reply.
///
/// A reply is about the key you just pressed and you were looking at the screen when you pressed
/// it. An alert is about something you did not do, so the six seconds start from a moment nobody
/// chose — and if you looked away for one sentence you have missed it entirely.
const ALERT_TTL: std::time::Duration = std::time::Duration::from_secs(20);

/// When a progress row is given up on.
///
/// Progress has no timer of its own: it lives until `notify.done` takes it away, because that is
/// the whole difference between a state and a message. This is only the backstop for a plugin that
/// crashed between the two — without it, one failed pull leaves `pulling…` on screen until restart.
const PROGRESS_ABANDONED: std::time::Duration = std::time::Duration::from_secs(60);

/// What is happening and what just happened, above the bottom-most content.
///
/// Two lists in one corner, because they are two different claims and only one of them is about to
/// stop being true. Progress sits above — it is what the program is *doing*, and it will change on
/// its own — and messages below it, nearest the composer, because they are answers to a key.
fn draw_notifications(frame: &mut Frame, area: Rect, mirror: &Mirror, theme: &Theme) {
    /// At most one reply and two alerts. The stack used to be three of anything, which meant a
    /// burst of replies could push the one piece of news off the top of it.
    const MAX: usize = 3;

    let ttl = |n: &crate::mirror::Notice| match n.kind {
        neosh_proto::NoticeKind::Alert => ALERT_TTL,
        _ => NOTIFICATION_TTL,
    };
    let live: Vec<&crate::mirror::Notice> =
        mirror.messages.iter().rev().filter(|n| n.at.elapsed() < ttl(n)).take(MAX).collect();
    // Progress in insertion-independent key order, so two operations running at once do not swap
    // rows every time one of them ticks.
    let running: Vec<&str> = mirror
        .progress
        .values()
        .filter(|(_, at)| at.elapsed() < PROGRESS_ABANDONED)
        .map(|(text, _)| text.as_str())
        .collect();
    if (live.is_empty() && running.is_empty()) || area.height == 0 {
        return;
    }

    // Newest at the bottom, older above it and dimmed — the same ordering as the toast stack it
    // replaces, without the choreography. A repeat is a count on the end rather than a second row.
    let mut lines: Vec<Line> = Vec::with_capacity(running.len() + live.len());
    for text in &running {
        lines.push(Line::from(Span::styled(
            format!(" {text} "),
            theme.style("Status.Pending").add_modifier(ratatui::style::Modifier::DIM),
        )));
    }
    let count = live.len();
    for (i, n) in live.iter().rev().enumerate() {
        let group = match n.level {
            neosh_proto::MessageLevel::Error => "Diagnostic.Error",
            neosh_proto::MessageLevel::Warn => "Diagnostic.Warn",
            neosh_proto::MessageLevel::Info => "Diagnostic.Info",
        };
        let mut style = theme.style(group);
        if i + 1 < count {
            style = style.add_modifier(ratatui::style::Modifier::DIM);
        }
        let text = match n.repeats {
            0 | 1 => format!(" {} ", n.text),
            r => format!(" {} ×{r} ", n.text),
        };
        lines.push(Line::from(Span::styled(text, style)));
    }

    // Right-aligned, hugging the bottom, so it overlays chrome rather than reflowing it: a message
    // that pushed the composer down would move the thing the user is typing into.
    let rows = (lines.len() as u16).min(area.height);
    let widest = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| width(&s.content)).sum::<usize>())
        .max()
        .unwrap_or(0)
        .min(area.width as usize) as u16;
    if widest == 0 {
        return;
    }
    // Measured before truncating, then truncated to what fits, so every row is the same width and
    // the block reads as one panel rather than a ragged edge.
    for line in &mut lines {
        for span in &mut line.spans {
            let fitted = truncate_to_width(&span.content, widest as usize).to_string();
            span.content = fitted.into();
        }
    }
    // Only the last `rows` fit. Dropping from the front keeps the newest, which is the one that
    // matters — and progress is at the front, so a screen too short for both shows the answers.
    if lines.len() > rows as usize {
        lines.drain(..lines.len() - rows as usize);
    }
    let rect = Rect {
        x: area.x + area.width.saturating_sub(widest),
        y: area.y + area.height.saturating_sub(rows + 1),
        width: widest,
        height: rows,
    };

    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(Paragraph::new(lines), rect);
}

trait LayoutExt {
    fn layout_is_float(&self) -> bool;
    /// Whether long lines wrap rather than clip.
    fn wraps(&self) -> bool;
    /// Whether an over-long buffer shows its end rather than its beginning.
    fn follows_tail(&self) -> bool;
}

impl LayoutExt for crate::mirror::MirrorWindow {
    fn layout_is_float(&self) -> bool {
        matches!(self.layout, WindowLayout::Float { .. })
    }

    /// Prose wraps; everything else clips.
    ///
    /// The main dock always, and any dock that asked. By default a side panel clips because a long
    /// path would reflow every row below it, and a status line clips because wrapping would eat the
    /// screen — but a text field like the composer is prose, and it opts in with `wrap`. A float
    /// that genuinely wants wrapped prose knows its own width — it set it — and can wrap its text
    /// before writing it.
    fn wraps(&self) -> bool {
        matches!(
            self.layout,
            WindowLayout::Docked { dock: Dock::Main, .. }
                | WindowLayout::Docked { wrap: Some(true), .. }
        )
    }

    /// Whether an over-long buffer shows its end rather than its beginning.
    ///
    /// A transcript does; a list does not. Tail-following a picker would hide its title and filter
    /// line, which are the two rows it cannot do without.
    fn follows_tail(&self) -> bool {
        matches!(self.layout, WindowLayout::Docked { dock: Dock::Main, .. })
    }
}
