//! Text that moves.
//!
//! # Why this lives in the frontend
//!
//! Because it is display maths. A shimmer is a per-cell colour computed from a clock, and both
//! halves of that — what colour a cell can be, and how often a frame may be drawn — are things only
//! the thing holding the terminal knows. The core stores an [`Animation`] on a highlight group and
//! forwards it; nothing above this file has an opinion about frames.
//!
//! # Why it degrades rather than switching off
//!
//! A sweep rendered as a colour gradient needs truecolor. Without it, the same band is rendered as
//! bold — which is one bit instead of twenty-four, and still unmistakably a thing moving left to
//! right. The alternative, showing nothing, would make "is it working?" unanswerable on exactly the
//! terminals where you are least sure.

use std::time::Instant;

use neosh_proto::Animation;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;

/// When this process started.
///
/// One clock for every animation on screen, so two shimmering rows sweep in step instead of
/// drifting apart at whatever rate each of them happened to be created.
fn epoch() -> Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// How far through its cycle an animation is, in `0.0..1.0`.
fn phase(period_ms: u32) -> f32 {
    let ms = epoch().elapsed().as_millis() as f32;
    (ms % period_ms as f32) / period_ms as f32
}

/// Blend `from` toward `to`. `t` is 0 at `from`, 1 at `to`.
fn blend(from: (u8, u8, u8), to: (u8, u8, u8), t: f32) -> Color {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t.clamp(0.0, 1.0)).round() as u8;
    Color::Rgb(mix(from.0, to.0), mix(from.1, to.1), mix(from.2, to.2))
}

/// The colour a style is drawn in, when it is one we can interpolate from.
///
/// `Color::Reset` means "whatever the terminal's default is", which is a value this process does not
/// have and cannot query portably. Rather than guess at it — and get the direction of the blend
/// wrong on half of all themes — those runs take the attribute path instead.
fn base_rgb(style: &Style) -> Option<(u8, u8, u8)> {
    match style.fg {
        Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
        _ => None,
    }
}

/// How bright the band is at a given distance from its centre, in half-widths.
///
/// A raised cosine rather than a linear ramp or a hard edge: the eye reads a hard edge as a second
/// object travelling along the text, and a linear ramp as a triangle. This reads as light.
fn band(distance: f32, half_width: f32) -> f32 {
    if distance > half_width {
        return 0.0;
    }
    0.5 * (1.0 + (std::f32::consts::PI * (distance / half_width)).cos())
}

/// Whether the frame being drawn has anything moving in it.
///
/// Set by [`animate`] and taken by the frontend after each draw, which is what decides whether to
/// keep asking for frames. Deliberately a property of *what was actually drawn* rather than of what
/// exists: an animated mark scrolled off the screen must stop costing frames, and the only code
/// that knows it was skipped is the code that would have drawn it.
static DREW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the last frame had motion in it, clearing the flag for the next one.
pub fn take_drew_animation() -> bool {
    DREW.swap(false, std::sync::atomic::Ordering::Relaxed)
}

/// Render one run of text with an animation applied.
///
/// Returns one span per grapheme cluster for a sweep, and a single span for anything uniform —
/// splitting text that does not need splitting would multiply the span count of every status line
/// for no visible difference.
pub fn animate(text: &str, base: Style, anim: Animation, truecolor: bool) -> Vec<Span<'static>> {
    DREW.store(true, std::sync::atomic::Ordering::Relaxed);
    match anim {
        Animation::Pulse { .. } => {
            let t = 0.5 * (1.0 - (std::f32::consts::TAU * phase(anim.period_ms())).cos());
            vec![Span::styled(text.to_string(), brighten(base, t, truecolor))]
        }
        Animation::Shimmer { .. } => {
            // Measured in grapheme clusters, so the band moves at a constant *visual* rate — a
            // sweep timed in bytes crawls through CJK and sprints through ASCII.
            let clusters: Vec<&str> = text.graphemes(true).collect();
            if clusters.is_empty() {
                return Vec::new();
            }
            // The band starts off the left edge and ends off the right, so the run spends part of
            // every cycle entirely unlit. Without the padding it would strobe: the moment the band
            // left one end it would already be entering the other.
            let half_width = 4.0f32;
            let pad = half_width + 1.0;
            let span = clusters.len() as f32 + pad * 2.0;
            let centre = phase(anim.period_ms()) * span - pad;

            clusters
                .iter()
                .enumerate()
                .map(|(i, g)| {
                    let t = band((i as f32 - centre).abs(), half_width);
                    Span::styled((*g).to_string(), brighten(base, t, truecolor))
                })
                .collect()
        }
    }
}

/// Lift a style toward "lit", by `t` in `0.0..1.0`.
fn brighten(base: Style, t: f32, truecolor: bool) -> Style {
    match (truecolor, base_rgb(&base)) {
        (true, Some(rgb)) => base.fg(blend(rgb, (255, 255, 255), t * 0.85)),
        // One bit instead of twenty-four. The threshold is past halfway so the lit part stays
        // narrower than the unlit part, which is what makes it read as a moving highlight rather
        // than as text that keeps changing its mind.
        _ => {
            if t > 0.55 {
                base.remove_modifier(Modifier::DIM).add_modifier(Modifier::BOLD)
            } else {
                base
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled() -> Style {
        Style::default().fg(Color::Rgb(100, 100, 100))
    }

    #[test]
    fn a_sweep_is_one_span_per_grapheme_and_leaves_the_text_alone() {
        // The text must survive being cut up: a shimmer that splits a family emoji into its
        // component people is a shimmer nobody will turn on twice.
        let text = "ab\u{1f469}\u{200d}\u{1f467}cd";
        let spans = animate(text, styled(), Animation::Shimmer { period_ms: 1000 }, true);
        let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, text);
        assert_eq!(spans.len(), text.graphemes(true).count());
    }

    #[test]
    fn without_truecolor_it_is_an_attribute_rather_than_nothing() {
        // "Is it working?" has to be answerable on the terminals where you are least sure.
        let spans = animate("working", styled(), Animation::Shimmer { period_ms: 1000 }, false);
        assert!(spans.iter().all(|s| s.style.fg == Some(Color::Rgb(100, 100, 100))));
        // Over a whole cycle some cluster is lit; at any instant most are not.
        assert!(spans.len() > 1);
    }

    #[test]
    fn the_band_is_brightest_at_its_centre_and_dark_beyond_its_edge() {
        assert!((band(0.0, 4.0) - 1.0).abs() < 1e-6);
        assert_eq!(band(4.0, 4.0), 0.0);
        assert_eq!(band(9.0, 4.0), 0.0);
        assert!(band(1.0, 4.0) > band(3.0, 4.0), "it falls off away from the centre");
    }

    #[test]
    fn a_run_with_no_colour_to_blend_from_is_not_guessed_at() {
        // `Color::Reset` is whatever the terminal decided, which this process cannot ask about.
        // Blending from a guess gets the direction wrong on half of all themes.
        let plain = Style::default();
        let spans = animate("hi", plain, Animation::Pulse { period_ms: 1000 }, true);
        assert!(spans[0].style.fg.is_none(), "no colour was invented");
    }

    #[test]
    fn a_period_of_zero_does_not_divide_by_zero() {
        // It arrives over a wire from a plugin, so it is input, not a constant.
        let spans = animate("hi", styled(), Animation::Shimmer { period_ms: 0 }, true);
        assert_eq!(spans.len(), 2);
    }
}
