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

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use neosh_proto::{Animation, ExtmarkId, FrameSet, NamespaceId};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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

fn moving() {
    DREW.store(true, std::sync::atomic::Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// When a mark first appeared
// ---------------------------------------------------------------------------

/// How long after a mark first appears it may still be considered new.
///
/// Past this it is history, and history does not flash. Also what bounds the table below: an entry
/// older than this cannot affect anything, so it is dropped rather than kept.
const YOUNG: Duration = Duration::from_secs(5);

/// How many marks may appear at once and still count as *events*.
///
/// A flash says "this just happened", and one row lighting up says it clearly. Twenty rows lighting
/// up together says nothing at all — it is a redraw, which is what republishing a conversation on
/// reattach looks like from here, and a transcript that strobes every time you switch to it is the
/// worst version of this feature. So a burst is recognised and none of it flashes.
const BURST: usize = 3;

/// The window inside which arrivals count as one burst.
const TOGETHER: Duration = Duration::from_millis(60);

#[derive(Default)]
struct Seen {
    /// The highest id handed out per namespace. Ids only ever increase, so anything at or below
    /// this that is not in `at` is a mark from before we started paying attention — which is how
    /// scrolling an old row back into view is told apart from a new row arriving, without keeping
    /// every id ever seen.
    high: HashMap<u32, u32>,
    /// When each young mark turned up, and whether it is allowed to flash.
    at: HashMap<(u32, u32), (Instant, bool)>,
    /// The most recent arrival, and how many came with it.
    burst: Option<(Instant, usize)>,
}

fn seen() -> &'static Mutex<Seen> {
    static SEEN: std::sync::OnceLock<Mutex<Seen>> = std::sync::OnceLock::new();
    SEEN.get_or_init(Mutex::default)
}

/// When this mark first turned up, if it is new enough for that to mean anything.
///
/// `None` for a mark that was already here — which is the answer for almost every mark, almost
/// always. Only the frontend calls this, once per animated row per frame.
pub fn first_seen(ns: NamespaceId, id: ExtmarkId) -> Option<Instant> {
    let now = Instant::now();
    let mut s = seen().lock().ok()?;
    let key = (ns.0, id.0);

    if let Some(&(at, flashing)) = s.at.get(&key) {
        return flashing.then_some(at);
    }

    let high = s.high.entry(ns.0).or_default();
    if id.0 <= *high {
        // Older than anything we have a record of: it was on screen before, and it has already had
        // whatever moment it was going to get.
        return None;
    }
    *high = id.0;

    let count = match s.burst {
        Some((at, n)) if now.duration_since(at) < TOGETHER => n + 1,
        _ => 1,
    };
    s.burst = Some((now, count));
    let flashing = count <= BURST;

    s.at.retain(|_, (at, _)| now.duration_since(*at) < YOUNG);
    s.at.insert(key, (now, flashing));
    flashing.then_some(now)
}

/// Forget everything. For tests, which would otherwise share one table.
#[cfg(test)]
fn forget() {
    if let Ok(mut s) = seen().lock() {
        *s = Seen::default();
    }
}

/// The glyphs a frame set is drawn with.
///
/// All one column wide, which is the only property this list has to have — [`fit`] enforces it
/// anyway, but a set that needed enforcing would look wrong on the way past.
fn frames_of(set: FrameSet) -> &'static [&'static str] {
    match set {
        FrameSet::Braille => &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
        FrameSet::Dots => &["⠁", "⠂", "⠄", "⡀", "⢀", "⠠", "⠐", "⠈"],
        FrameSet::Bars => &["▁", "▂", "▃", "▄", "▅", "▆", "▇", "▆", "▅", "▄", "▃", "▂"],
        FrameSet::Arc => &["◐", "◓", "◑", "◒"],
        FrameSet::Corners => &["▖", "▘", "▝", "▗"],
    }
}

/// A frame, cut or padded to the width of what it is standing in for.
///
/// The whole safety property of [`Animation::Frames`]: whatever the frame is, the cell it occupies
/// is the width of the text underneath it, so the columns to its right never move. A spinner that
/// shifts the rest of the row by one every tenth of a second is worse than no spinner.
fn fit(frame: &str, want: usize) -> String {
    let mut out = String::new();
    let mut have = 0usize;
    for g in frame.graphemes(true) {
        let w = UnicodeWidthStr::width(g);
        if have + w > want {
            break;
        }
        out.push_str(g);
        have += w;
    }
    out.push_str(&" ".repeat(want.saturating_sub(have)));
    out
}

/// How lit a flash is right now, or `None` if it is not one or is over.
///
/// Split out because the row's band and the runs on it need the same answer: the band is what
/// carries a flash, and what it lights is every span on the row. Setting [`DREW`] is part of the
/// answer — asking is what draws it.
pub fn flash_amount(anim: Animation, since: Instant) -> Option<f32> {
    let Animation::Flash { ms } = anim else { return None };
    let e = since.elapsed().as_secs_f32() * 1000.0 / ms.max(1) as f32;
    if e >= 1.0 {
        return None;
    }
    moving();
    // Out rather than in, and squared: a landing is bright immediately and then gets out of the
    // way. Easing in would read as the row arriving twice.
    Some((1.0 - e) * (1.0 - e))
}

/// Lift a style toward lit, for a caller that already knows how far.
pub fn lift(style: Style, t: f32, truecolor: bool) -> Style {
    brighten(style, t, truecolor)
}

/// Render one run of text with an animation applied.
///
/// Returns one span per grapheme cluster for a sweep, and a single span for anything uniform —
/// splitting text that does not need splitting would multiply the span count of every status line
/// for no visible difference.
///
/// `since` is when the mark carrying this animation first appeared, for the one animation that
/// needs to know — see [`Animation::Flash`]. `None` from every caller that has no mark to ask
/// about, and a flash with nothing to count from simply does not fire.
pub fn animate(
    text: &str,
    base: Style,
    anim: Animation,
    truecolor: bool,
    since: Option<Instant>,
) -> Vec<Span<'static>> {
    // Set per arm rather than here: a flash that has finished draws its run exactly as it would
    // have been drawn anyway, and saying "something moved" about it would keep the frame ticker
    // alive over a transcript that has been still for an hour.
    match anim {
        Animation::Flash { .. } => {
            match since.and_then(|at| flash_amount(anim, at)) {
                Some(t) => vec![Span::styled(text.to_string(), brighten(base, t, truecolor))],
                None => vec![Span::styled(text.to_string(), base)],
            }
        }
        Animation::Frames { set, .. } => {
            moving();
            let frames = frames_of(set);
            let at = (phase(anim.period_ms()) * frames.len() as f32) as usize;
            let frame = frames[at.min(frames.len() - 1)];
            vec![Span::styled(fit(frame, UnicodeWidthStr::width(text)), base)]
        }
        Animation::Pulse { .. } => {
            moving();
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
            moving();
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
        let spans = animate(text, styled(), Animation::Shimmer { period_ms: 1000 }, true, None);
        let rebuilt: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rebuilt, text);
        assert_eq!(spans.len(), text.graphemes(true).count());
    }

    #[test]
    fn without_truecolor_it_is_an_attribute_rather_than_nothing() {
        // "Is it working?" has to be answerable on the terminals where you are least sure.
        let spans =
            animate("working", styled(), Animation::Shimmer { period_ms: 1000 }, false, None);
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
        let spans = animate("hi", plain, Animation::Pulse { period_ms: 1000 }, true, None);
        assert!(spans[0].style.fg.is_none(), "no colour was invented");
    }

    /// The tables `first_seen` keeps are global, because the clock behind every animation is — and
    /// so is `DREW`. Two tests minting marks at once would each see the other's as part of their
    /// burst, and a test asking "did anything move" would be answered about somebody else's frame.
    fn alone() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        forget();
        take_drew_animation();
        g
    }

    #[test]
    fn a_frame_is_the_width_of_what_it_stands_in_for() {
        // The whole safety property: a spinner cannot move the column after it. Every set, every
        // frame, against a one-column run — which is what the mark beside a card is.
        for set in [
            FrameSet::Braille,
            FrameSet::Dots,
            FrameSet::Bars,
            FrameSet::Arc,
            FrameSet::Corners,
        ] {
            for frame in frames_of(set) {
                assert_eq!(
                    UnicodeWidthStr::width(fit(frame, 1).as_str()),
                    1,
                    "{set:?} drew {frame:?} at the wrong width"
                );
            }
        }
        // And a frame too wide for its run is cut rather than allowed to push.
        assert_eq!(UnicodeWidthStr::width(fit("⠋⠙", 1).as_str()), 1);
        // A run with no room at all takes nothing, rather than one column of somebody else's.
        assert_eq!(fit("⠋", 0), "");
    }

    #[test]
    fn a_frame_replaces_the_text_without_touching_the_line() {
        let _g = alone();
        let spans = animate("\u{25b8}", styled(), Animation::Frames {
            set: FrameSet::Braille,
            period_ms: 800,
        }, true, None);
        let drawn: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(frames_of(FrameSet::Braille).contains(&drawn.as_str()), "{drawn:?}");
        assert!(take_drew_animation(), "a spinner is a reason to ask for another frame");
    }

    #[test]
    fn a_flash_with_nothing_to_count_from_does_not_fire() {
        let _g = alone();
        let spans = animate("landed", styled(), Animation::Flash { ms: 300 }, true, None);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].style.fg, Some(Color::Rgb(100, 100, 100)), "unlit");
        assert!(!take_drew_animation(), "and it does not ask for another frame");
    }

    #[test]
    fn a_flash_is_over_when_it_is_over() {
        let _g = alone();
        let anim = Animation::Flash { ms: 40 };
        let now = Instant::now();
        assert!(flash_amount(anim, now).is_some_and(|t| t > 0.9), "brightest at the start");
        assert!(take_drew_animation());

        // Past its window it is not motion any more — which is what stops a transcript full of
        // landed cards from keeping the frame ticker alive for ever.
        let old = now - Duration::from_millis(80);
        assert_eq!(flash_amount(anim, old), None);
        assert!(!take_drew_animation());
    }

    #[test]
    fn a_mark_flashes_once_and_a_mark_that_was_already_here_never_does() {
        let _g = alone();
        let (ns, id) = (NamespaceId(7), ExtmarkId(20));
        let first = first_seen(ns, id).expect("a mark nobody has seen before is news");
        assert_eq!(first_seen(ns, id), Some(first), "asking twice is the same moment, not a new one");

        // Scrolling an older row back into view is not an arrival. Ids only ever increase, so an id
        // below the high-water mark is one from before we were watching.
        assert_eq!(first_seen(ns, ExtmarkId(3)), None);
    }

    #[test]
    fn a_burst_of_marks_is_a_redraw_and_none_of_it_flashes() {
        let _g = alone();
        // Republishing a conversation on reattach mints every mark in it at once. One row lighting
        // up is an event; twenty is a strobe, and it is the same feature that produced both.
        let ns = NamespaceId(9);
        let lit = (1..=20).filter(|i| first_seen(ns, ExtmarkId(*i)).is_some()).count();
        assert_eq!(lit, BURST, "only the first few of a burst are treated as news");
    }

    #[test]
    fn a_period_of_zero_does_not_divide_by_zero() {
        // It arrives over a wire from a plugin, so it is input, not a constant.
        let spans = animate("hi", styled(), Animation::Shimmer { period_ms: 0 }, true, None);
        assert_eq!(spans.len(), 2);
    }
}
