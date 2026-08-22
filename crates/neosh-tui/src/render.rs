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
    Anchor, Dock, Extent, LineRender, VirtTextPos, WindowId, WindowLayout,
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

pub fn resolve_layout(mirror: &Mirror, area: Rect) -> Vec<(WindowId, Rect)> {
    resolve_layout_with_rules(mirror, area).0
}

/// Layout, plus the columns where a separator should be drawn.
pub fn resolve_layout_with_rules(
    mirror: &Mirror,
    area: Rect,
) -> (Vec<(WindowId, Rect)>, Vec<Rect>) {
    let mut out = Vec::new();
    let mut rules: Vec<Rect> = Vec::new();
    let mut main = area;

    let dock_of = |w: &WindowId| match mirror.windows.get(w).map(|w| &w.layout) {
        Some(WindowLayout::Docked { dock, size, .. }) => Some((*dock, *size)),
        _ => None,
    };

    for dock in [Dock::Left, Dock::Right, Dock::Bottom] {
        // Every window on this edge, not just the first. Stacking them is what lets a status line
        // sit under the composer, and what stops a plugin's panel from being silently invisible
        // because something was already docked there.
        let docked: Vec<WindowId> = mirror
            .window_order
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
                    let rule = if avail > w { DOCK_RULE } else { 0 };
                    let (a, b) = if dock == Dock::Left {
                        if rule > 0 {
                            rules.push(Rect { x: main.x + w, width: 1, ..main });
                        }
                        (Rect { width: w, ..main }, Rect {
                            x: main.x + w + rule,
                            width: main.width - w - rule,
                            ..main
                        })
                    } else {
                        let edge = main.x + main.width - w;
                        if rule > 0 {
                            rules.push(Rect { x: edge - 1, width: 1, ..main });
                        }
                        (
                            Rect { x: edge, width: w, ..main },
                            Rect { width: main.width - w - rule, ..main },
                        )
                    };
                    out.push((win, a));
                    main = b;
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
                            let rows = rendered_lines(
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

    if let Some(win) = mirror.window_order.iter().find(|w| dock_of(w).map(|d| d.0) == Some(Dock::Main))
    {
        out.push((*win, main));
    }

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

    (out, rules)
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Draw one frame, and report where the caret belongs.
///
/// The caret is not decoration. Without one the composer gives no sign that typing goes anywhere,
/// and a terminal program with no visible insertion point reads as frozen.
pub fn draw(frame: &mut Frame, mirror: &Mirror, theme: &Theme) -> Option<(u16, u16)> {
    let area = frame.area();
    let (rects, rules) = resolve_layout_with_rules(mirror, area);

    // Drawn before the windows so a float still covers it, and before content so nothing has to
    // know the rule is there.
    let rule_style = theme.style("Separator");
    for r in &rules {
        for y in r.y..r.y.saturating_add(r.height) {
            if let Some(cell) = frame.buffer_mut().cell_mut((r.x, y)) {
                cell.set_symbol("│");
                cell.set_style(rule_style);
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
                b = b.border_style(
                    theme.style(config.border_hl.as_deref().unwrap_or("Float.Border")),
                );
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
        let rendered = rendered_lines(mirror, w, theme, inner.width);

        // Follow the tail. Taking the *first* n lines shows a long conversation's opening and
        // never its answer, which is the wrong end of every transcript ever written. An explicit
        // `top_line` still means "start here", so paging keeps working.
        let height = inner.height as usize;
        let lines: Vec<Line> = if w.follows_tail() && w.top_line == 0 && rendered.len() > height {
            rendered[rendered.len() - height..].to_vec()
        } else {
            rendered.into_iter().take(height).collect()
        };

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

        frame.render_widget(Paragraph::new(lines).style(theme.style("Normal")), inner);
    }

    draw_notifications(frame, area, mirror, theme);

    // Raw-cell surfaces blit last: a plugin claimed those cells and owns them outright.
    let caret = caret_position(mirror, &rects, theme);

    for surface in mirror.surfaces.values() {
        let Some((_, host)) = rects.iter().find(|(id, _)| *id == surface.win) else { continue };
        for cell in &surface.cells {
            let x = host.x + surface.rect.col + cell.col;
            let y = host.y + surface.rect.row + cell.row;
            if x >= host.x + host.width || y >= host.y + host.height {
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
                target.set_style(Style::default().add_modifier(crate::theme::modifiers(&cell.attrs)));
            }
        }
    }

    caret
}

/// Every screen line a window's visible buffer range turns into, marks and wrapping applied.
///
/// Shared by drawing and by the caret, which is the point: gravity moves content down by however
/// many rows it falls short, and the caret has to move by the *same* number. Two calculations of
/// "how many lines is this" would agree until the first wrapped line, and then quietly stop.
fn rendered_lines(
    mirror: &Mirror,
    w: &crate::mirror::MirrorWindow,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    // Wrapping is per window: the chat has to wrap or a long answer is silently cut off at the
    // right edge, while the sidebar and the status line must clip or one long path would push
    // everything below it off the screen.
    let wraps = w.wraps();
    mirror
        .buffers
        .get(&w.buf)
        .map(|b| {
            b.lines
                .iter()
                .skip(w.top_line as usize)
                .flat_map(|l| render_line_in(l, theme, Some(width as usize)))
                .flat_map(|l| if wraps { wrap_line(&l, width as usize) } else { vec![l] })
                .map(|l| fill_to_edge(l, width as usize))
                .collect()
        })
        .unwrap_or_default()
}

/// Where the caret goes, in screen cells.
///
/// The focused window if there is one, else the bottom-most docked window — which is the composer,
/// and is where typing lands when nothing else has claimed the keyboard. Returns `None` when the
/// target is off-screen, so the caller hides it rather than parking it in a corner.
fn caret_position(
    mirror: &Mirror,
    rects: &[(WindowId, Rect)],
    theme: &Theme,
) -> Option<(u16, u16)> {
    let target = mirror.focus.or_else(|| {
        // The composer: the last docked-bottom window, matching how the host stacks them.
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
    })?;

    let (_, rect) = rects.iter().find(|(id, _)| *id == target)?;
    let w = mirror.windows.get(&target)?;
    let inner = if w.layout_is_float() {
        // Inside the border, when there is one.
        let has_border = matches!(&w.layout, WindowLayout::Float { config }
            if !matches!(config.border, neosh_proto::BorderStyle::None));
        if has_border {
            Rect {
                x: rect.x.saturating_add(1),
                y: rect.y.saturating_add(1),
                width: rect.width.saturating_sub(2),
                height: rect.height.saturating_sub(2),
            }
        } else {
            *rect
        }
    } else {
        *rect
    };
    if inner.width == 0 || inner.height == 0 {
        return None;
    }

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

    // Saturating throughout: a row far below `top_line` must clamp to "off-screen" rather than
    // wrap around into a plausible-looking coordinate.
    let mut line = u16::try_from(row.saturating_sub(w.top_line)).unwrap_or(u16::MAX);
    line = line.saturating_add(virt_rows_before(buffer, w.top_line, row));
    // Content pushed down by gravity takes the caret with it.
    if let WindowLayout::Docked { gravity: neosh_proto::Gravity::End, .. } = &w.layout {
        let rows = rendered_lines(mirror, w, theme, inner.width).len();
        line = line.saturating_add((inner.height as usize).saturating_sub(rows) as u16);
    }
    if w.wraps() {
        // On a wrapped line the caret moves down as well as along.
        line = line.saturating_add(column / inner.width);
        column %= inner.width;
    }
    if line >= inner.height || column >= inner.width {
        return None;
    }
    Some((inner.x + column, inner.y + line))
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

/// How many extra screen rows the virtual lines between `top` and `row` take up.
///
/// `Above` on a row at or before the caret pushes it down; `Below` only counts for rows strictly
/// before it, since a line under the caret's own row is drawn after the caret.
fn virt_rows_before(buffer: &crate::mirror::MirrorBuffer, top: u32, row: u32) -> u16 {
    let mut extra: usize = 0;
    for (i, line) in buffer.lines.iter().enumerate().skip(top as usize) {
        let i = i as u32;
        if i > row {
            break;
        }
        for m in &line.marks {
            if m.opts.virt_text.is_empty() {
                continue;
            }
            match m.opts.virt_text_pos {
                VirtTextPos::Above => extra += 1,
                VirtTextPos::Below if i < row => extra += 1,
                _ => {}
            }
        }
    }
    u16::try_from(extra).unwrap_or(u16::MAX)
}

/// How long a notification stays on screen.
///
/// There is no frame loop, so nothing repaints purely to expire one: a message outlives its welcome
/// until the next real mutation. That is the honest trade for having no timer — and in a session
/// where anything at all is happening, the next frame is milliseconds away.
const NOTIFICATION_TTL: std::time::Duration = std::time::Duration::from_secs(6);

/// The most recent notifications, above the bottom-most content.
///
/// Without this every `neosh.notify` — including the host's own "press ^C again to quit" — is
/// collected, capped, and never shown. A program whose only feedback channel is invisible is a
/// program that appears not to respond.
fn draw_notifications(frame: &mut Frame, area: Rect, mirror: &Mirror, theme: &Theme) {
    const MAX: usize = 3;

    let live: Vec<_> = mirror
        .messages
        .iter()
        .rev()
        .filter(|(_, _, at)| at.elapsed() < NOTIFICATION_TTL)
        .take(MAX)
        .collect();
    if live.is_empty() || area.height == 0 {
        return;
    }

    // Right-aligned, hugging the bottom, so it overlays chrome rather than reflowing it: a message
    // that pushed the composer down would move the thing the user is typing into.
    let rows = (live.len() as u16).min(area.height);
    let widest = live
        .iter()
        .map(|(_, text, _)| width(text) + 2)
        .max()
        .unwrap_or(0)
        .min(area.width as usize) as u16;
    if widest == 0 {
        return;
    }
    let rect = Rect {
        x: area.x + area.width.saturating_sub(widest),
        y: area.y + area.height.saturating_sub(rows + 1),
        width: widest,
        height: rows,
    };

    frame.render_widget(ratatui::widgets::Clear, rect);
    // Newest at the bottom, older above it and dimmed — the same ordering as the toast stack it
    // replaces, without the choreography.
    let lines: Vec<Line> = live
        .iter()
        .rev()
        .enumerate()
        .map(|(i, (level, text, _))| {
            let group = match level {
                neosh_proto::MessageLevel::Error => "Diagnostic.Error",
                neosh_proto::MessageLevel::Warn => "Diagnostic.Warn",
                neosh_proto::MessageLevel::Info => "Diagnostic.Info",
            };
            let mut style = theme.style(group);
            if i + 1 < live.len() {
                style = style.add_modifier(ratatui::style::Modifier::DIM);
            }
            Line::from(Span::styled(
                format!(" {} ", truncate_to_width(text, widest.saturating_sub(2) as usize)),
                style,
            ))
        })
        .collect();
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
