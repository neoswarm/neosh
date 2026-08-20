//! The semantic vocabulary, and the two themes that fill it in.
//!
//! # Why the core owns a palette at all
//!
//! Because a link has to resolve. A plugin writes `hl.define("PickerSelected", { link: "CursorLine" })`
//! precisely so it inherits whatever the active theme decided — and if `CursorLine` does not exist,
//! the chain dead-ends and the group renders plain. That is not a theming shortfall, it is an
//! invisible selection highlight. So the set of names below is a *contract*: every one of them
//! resolves in every theme, and a plugin may link to any of them.
//!
//! # The rules the colours follow
//!
//! - **Meaning survives losing colour.** Every state pairs a hue with a glyph or an attribute, so a
//!   16-colour terminal, `NO_COLOR`, or a colour-blind reader loses speed, not information.
//! - **Attributes carry prominence, not decoration.** Bold means *needs you*. Normal means *this is
//!   where you are*. Dim means *receding*. Reverse is reserved for selection and for the one-shot
//!   "it moved there" flash, so it always means "look here now".
//! - **Three hues do the work.** Cyan for in motion, amber for act now, red for broken. Everything
//!   else is a shade of the text colour. A fourth hue would mean memorising a legend.
//!
//! Values are declared as truecolor and down-converted at the terminal boundary
//! (`neosh_tui::theme`), so nothing here reasons about what a terminal supports.

use neosh_proto::{Color, HighlightDef, HighlightSpec};

/// Which way round the terminal is.
///
/// Not detectable reliably — no terminal reports its background — so this is a setting with a
/// sensible default rather than a guess that is wrong half the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Variant {
    #[default]
    Dark,
    Light,
}

impl Variant {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            _ => None,
        }
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb { r, g, b }
}

/// The colours each variant uses, by role rather than by name.
struct Roles {
    /// Ordinary text. `Default` means "whatever the terminal already is", which is almost always
    /// what a user wants — overriding the background is how a theme ends up fighting the terminal.
    fg: Color,
    /// Receding text: metadata, paths, counts.
    muted: Color,
    /// Barely there: separators, placeholder glyphs.
    faint: Color,
    /// The row you are on.
    cursor_bg: Color,
    /// A filter match inside a row.
    match_bg: Color,
    /// In motion.
    active: Color,
    /// Act now.
    attention: Color,
    /// Broken.
    danger: Color,
    /// Finished well.
    success: Color,
    /// A plan, a proposal, something to read before acting.
    plan: Color,
    /// The one hue used for emphasis that is not a state.
    accent: Color,
    /// The favourite marker. Not a fourth state hue: the heart glyph carries the whole meaning,
    /// and the world has already decided what colour a heart is — painting it anything else reads
    /// as a decision the user has to decode.
    favorite: Color,
}

const DARK: Roles = Roles {
    fg: Color::Default,
    muted: rgb(0x8b, 0x94, 0xa3),
    faint: rgb(0x4b, 0x52, 0x63),
    cursor_bg: rgb(0x2a, 0x30, 0x3c),
    match_bg: rgb(0x3b, 0x45, 0x5c),
    active: rgb(0x38, 0xbd, 0xf8),
    attention: rgb(0xfc, 0xd3, 0x4d),
    danger: rgb(0xfc, 0xa5, 0xa5),
    success: rgb(0x6e, 0xe7, 0xb7),
    plan: rgb(0xc4, 0xb5, 0xfd),
    accent: rgb(0xa5, 0xb4, 0xfc),
    favorite: rgb(0xfb, 0x64, 0x7b),
};

const LIGHT: Roles = Roles {
    fg: Color::Default,
    muted: rgb(0x5b, 0x63, 0x71),
    faint: rgb(0x9c, 0xa3, 0xaf),
    cursor_bg: rgb(0xe5, 0xe9, 0xf0),
    match_bg: rgb(0xd6, 0xe2, 0xf5),
    active: rgb(0x0e, 0xa5, 0xe9),
    attention: rgb(0xd9, 0x77, 0x06),
    danger: rgb(0xb9, 0x1c, 0x1c),
    success: rgb(0x05, 0x96, 0x69),
    plan: rgb(0x7c, 0x3a, 0xed),
    accent: rgb(0x4f, 0x46, 0xe5),
    favorite: rgb(0xe1, 0x1d, 0x48),
};

fn fg(c: Color) -> HighlightSpec {
    HighlightSpec { fg: Some(c), ..HighlightSpec::default() }
}

fn bg(c: Color) -> HighlightSpec {
    HighlightSpec { bg: Some(c), ..HighlightSpec::default() }
}

fn bold(mut s: HighlightSpec) -> HighlightSpec {
    s.attrs.bold = true;
    s
}

fn dim(mut s: HighlightSpec) -> HighlightSpec {
    s.attrs.dim = true;
    s
}

fn italic(mut s: HighlightSpec) -> HighlightSpec {
    s.attrs.italic = true;
    s
}

fn underline(mut s: HighlightSpec) -> HighlightSpec {
    s.attrs.underline = true;
    s
}

fn struck(mut s: HighlightSpec) -> HighlightSpec {
    s.attrs.strikethrough = true;
    s
}

fn shimmer(mut s: HighlightSpec, period_ms: u32) -> HighlightSpec {
    s.animate = Some(neosh_proto::Animation::Shimmer { period_ms });
    s
}

fn pulse(mut s: HighlightSpec, period_ms: u32) -> HighlightSpec {
    s.animate = Some(neosh_proto::Animation::Pulse { period_ms });
    s
}

/// Every group a plugin may link to, with a concrete spec.
///
/// Kept as one list rather than scattered across the crates that use each group, because the whole
/// point is that the set is *closed and known*: a plugin author can read this and see exactly what
/// is safe to inherit from.
pub fn groups(variant: Variant) -> Vec<(&'static str, HighlightDef)> {
    let r = match variant {
        Variant::Dark => DARK,
        Variant::Light => LIGHT,
    };

    let spec = |s: HighlightSpec| HighlightDef::Spec { spec: s };
    let link = |to: &str| HighlightDef::Link { to: to.to_string() };

    vec![
        // ---- base ---------------------------------------------------------
        ("Normal", spec(fg(r.fg))),
        ("Comment", spec(dim(fg(r.muted)))),
        ("NonText", spec(dim(fg(r.faint)))),
        ("Title", spec(bold(fg(r.fg)))),
        // The row the cursor is on. Background rather than reverse: reverse would fight any
        // foreground the row already carries, and a list row usually carries one.
        ("CursorLine", spec(bg(r.cursor_bg))),
        ("Visual", spec(bg(r.match_bg))),
        ("Search", spec(bold(fg(r.active)))),
        ("Directory", spec(fg(r.accent))),
        ("Separator", spec(dim(fg(r.faint)))),
        ("Key", spec(bold(fg(r.accent)))),
        ("Accent", spec(fg(r.accent))),
        // ---- diagnostics --------------------------------------------------
        ("Diagnostic.Error", spec(bold(fg(r.danger)))),
        ("Diagnostic.Warn", spec(fg(r.attention))),
        ("Diagnostic.Info", spec(fg(r.active))),
        ("Diagnostic.Hint", spec(dim(fg(r.muted)))),
        ("Diagnostic.Ok", spec(fg(r.success))),
        // ---- floats -------------------------------------------------------
        ("Float.Border", spec(fg(r.faint))),
        ("Float.Title", spec(bold(fg(r.accent)))),
        // ---- the agent ----------------------------------------------------
        ("Agent.User", spec(bold(fg(r.accent)))),
        ("Agent.Assistant", spec(fg(r.fg))),
        // Reasoning is deliberately the quietest thing on screen: it is context, not the answer.
        ("Agent.Thinking", spec(italic(dim(fg(r.muted))))),
        ("Agent.Tool", spec(fg(r.active))),
        ("Agent.ToolError", spec(bold(fg(r.danger)))),
        ("Agent.Usage", spec(dim(fg(r.muted)))),
        // ---- markdown -----------------------------------------------------
        // An answer is prose with structure in it, and the structure should be visible without
        // being loud: the thing being read is the words. Hence one accent for headings and links,
        // one quiet colour for code, and attributes rather than colour for emphasis — a paragraph
        // where three words are a different hue is harder to read than one where they are bold.
        ("Markdown.Heading", spec(bold(fg(r.accent)))),
        ("Markdown.Bold", spec(bold(fg(r.fg)))),
        ("Markdown.Italic", spec(italic(fg(r.fg)))),
        // Struck-through text is text the answer has told you to disregard, so it reads at the
        // weight of something disregarded: the line through it *and* a step back on the ramp.
        ("Markdown.Strike", spec(struck(dim(fg(r.muted))))),
        ("Markdown.Code", spec(fg(r.active))),
        ("Markdown.Fence", spec(dim(fg(r.muted)))),
        ("Markdown.Bullet", spec(fg(r.accent))),
        ("Markdown.Quote", spec(dim(fg(r.faint)))),
        ("Markdown.Rule", link("Separator")),
        ("Markdown.Link", spec(underline(fg(r.accent)))),
        // ---- conversation state -------------------------------------------
        // One hue per meaning: in motion, act now, broken, done.
        ("Status.Working", spec(fg(r.active))),
        ("Status.Monitoring", spec(dim(fg(r.active)))),
        ("Status.Approval", spec(bold(fg(r.attention)))),
        ("Status.Input", spec(bold(fg(r.accent)))),
        ("Status.Plan", spec(fg(r.plan))),
        ("Status.Failed", spec(bold(fg(r.danger)))),
        ("Status.Done", spec(fg(r.success))),
        ("Status.Idle", spec(dim(fg(r.muted)))),
        // The two groups that move. Motion is reserved for "something is happening and you cannot
        // see it yet" — the one state where a still screen and a wedged program look identical.
        //
        // A sweep rather than a blink: a blink asks to be looked at every time it changes, and you
        // pay that attention whether or not there is news. A band travelling along a word reads as
        // aliveness at the edge of vision and costs nothing to ignore.
        ("Status.Streaming", spec(shimmer(fg(r.active), 2000))),
        // Slower and uniform, for waiting on something outside the program: a tool, a subprocess,
        // a person. Different shape, because it is a different kind of waiting.
        ("Status.Pending", spec(pulse(fg(r.attention), 2600))),
        // ---- version control ----------------------------------------------
        ("Git.Added", spec(fg(r.success))),
        ("Git.Modified", spec(fg(r.attention))),
        ("Git.Deleted", spec(fg(r.danger))),
        ("Git.Renamed", spec(fg(r.active))),
        ("Git.Untracked", spec(dim(fg(r.muted)))),
        ("Git.Ignored", spec(dim(fg(r.faint)))),
        ("Git.Conflict", spec(bold(fg(r.danger)))),
        ("Git.Branch", spec(fg(r.accent))),
        ("Git.Ahead", spec(fg(r.success))),
        ("Git.Behind", spec(fg(r.attention))),
        // ---- diffs --------------------------------------------------------
        ("Diff.Add", spec(fg(r.success))),
        ("Diff.Delete", spec(fg(r.danger))),
        ("Diff.Header", spec(bold(fg(r.accent)))),
        ("Diff.Hunk", spec(dim(fg(r.active)))),
        // ---- meters -------------------------------------------------------
        ("Meter.Fill", spec(fg(r.active))),
        ("Meter.Empty", spec(dim(fg(r.faint)))),
        // A context window nearly full is the one number that changes what you do next.
        ("Meter.Warn", spec(fg(r.attention))),
        ("Meter.Full", spec(bold(fg(r.danger)))),
        // ---- widgets ------------------------------------------------------
        // Named so widget authors inherit the theme instead of picking colours. `@neosh/api/ui`
        // links its own groups to these.
        ("Picker.Selected", link("CursorLine")),
        ("Picker.Match", link("Search")),
        ("Picker.Detail", link("Comment")),
        ("Sidebar.Heading", link("Title")),
        ("Sidebar.Dim", link("Comment")),
        ("Sidebar.Selected", link("CursorLine")),
        ("Sidebar.Favorite", spec(fg(r.favorite))),
        ("Status.Line", link("Comment")),
        // ---- the composer -------------------------------------------------
        // The field you type into is chrome, not content: it should read as furniture at the
        // bottom of the screen and never compete with what the model just said. Hence a rule at
        // the faintest step on the ramp, and a prompt that is the one accented thing down there.
        ("Composer.Rule", link("Separator")),
        ("Composer.Prompt", spec(bold(fg(r.accent)))),
        ("Composer.Placeholder", link("NonText")),
        ("Composer.Hint", link("Comment")),
        ("Composer.HintKey", spec(fg(r.muted))),
        // ---- provider marks -----------------------------------------------
        // One group per vendor, so a provider rail is legible at a glance and a theme can restyle
        // the lot. These are the *only* groups tied to something outside neosh, which is why they
        // are named after the company rather than after a role: there is no role to name, the
        // whole job is "looks like that vendor".
        //
        // Approximations of brand hues, pulled toward the palette so a rail of eleven marks still
        // looks like one program. A provider plugin adds its own group and nothing here changes.
        ("Brand.Anthropic", spec(fg(rgb(0xd9, 0x7a, 0x57)))),
        ("Brand.OpenAI", spec(fg(rgb(0x74, 0xaa, 0x9c)))),
        ("Brand.Google", spec(fg(rgb(0x66, 0x9d, 0xf6)))),
        ("Brand.OpenRouter", spec(fg(rgb(0x8b, 0x8b, 0xa7)))),
        ("Brand.Groq", spec(fg(rgb(0xf5, 0x5c, 0x36)))),
        ("Brand.DeepSeek", spec(fg(rgb(0x4d, 0x6b, 0xfe)))),
        ("Brand.XAI", spec(fg(r.fg))),
        ("Brand.Cursor", spec(fg(rgb(0x7c, 0x8b, 0xa1)))),
        ("Brand.Mistral", spec(fg(rgb(0xff, 0x8a, 0x00)))),
        ("Brand.Together", spec(fg(rgb(0x0f, 0x6f, 0xff)))),
        ("Brand.Fireworks", spec(fg(rgb(0xff, 0x5a, 0x8a)))),
        ("Brand.Cerebras", spec(fg(rgb(0xf2, 0x54, 0x54)))),
        // Anything on this machine. One group, because "local" is the distinction that matters.
        ("Brand.Local", spec(fg(r.success))),
        // ---- accounts ------------------------------------------------------
        // A plan and a key cost different things. Saying so in colour means the picker does not
        // have to say it in words on every row.
        ("Account.Plan", spec(fg(r.success))),
        ("Account.Key", spec(fg(r.accent))),
        ("Account.Local", spec(dim(fg(r.muted)))),
        ("Account.Missing", link("Diagnostic.Warn")),
    ]
}

/// Names every theme must define, for the test that keeps this a contract rather than a suggestion.
pub fn contract() -> Vec<&'static str> {
    groups(Variant::Dark).into_iter().map(|(n, _)| n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn both_variants_define_exactly_the_same_names() {
        // A group that exists only in one theme is a link that dead-ends the moment someone
        // switches, and a dead link renders plain rather than erroring.
        let dark: BTreeSet<_> = groups(Variant::Dark).into_iter().map(|(n, _)| n).collect();
        let light: BTreeSet<_> = groups(Variant::Light).into_iter().map(|(n, _)| n).collect();
        assert_eq!(dark, light);
    }

    #[test]
    fn every_link_resolves_within_the_palette() {
        for variant in [Variant::Dark, Variant::Light] {
            let all = groups(variant);
            let names: BTreeSet<_> = all.iter().map(|(n, _)| *n).collect();
            for (name, def) in &all {
                if let HighlightDef::Link { to } = def {
                    assert!(
                        names.contains(to.as_str()),
                        "{variant:?}: {name} links to {to}, which no theme defines"
                    );
                }
            }
        }
    }

    #[test]
    fn no_group_is_defined_twice() {
        for variant in [Variant::Dark, Variant::Light] {
            let all = groups(variant);
            let unique: BTreeSet<_> = all.iter().map(|(n, _)| *n).collect();
            assert_eq!(unique.len(), all.len(), "{variant:?} has a duplicate group");
        }
    }

    #[test]
    fn the_widget_groups_a_plugin_links_to_all_exist() {
        // These are the ones `@neosh/api/ui` and the bundled plugins inherit from. A missing one is
        // an invisible selection highlight, which is exactly the bug this palette was written for.
        let names: BTreeSet<_> = contract().into_iter().collect();
        for required in [
            "CursorLine",
            "Search",
            "Comment",
            "Title",
            "Normal",
            "Picker.Selected",
            "Picker.Match",
            "Sidebar.Heading",
            "Meter.Fill",
            "Meter.Empty",
            "Status.Working",
            "Git.Added",
            "Diff.Add",
        ] {
            assert!(names.contains(required), "{required} is missing from the palette");
        }
    }

    #[test]
    fn variants_parse_and_anything_else_is_rejected() {
        assert_eq!(Variant::parse("dark"), Some(Variant::Dark));
        assert_eq!(Variant::parse(" Light "), Some(Variant::Light));
        assert_eq!(Variant::parse("solarized"), None);
        assert_eq!(Variant::parse(""), None);
    }

    #[test]
    fn the_two_variants_actually_differ() {
        // A "light theme" that is the dark one with a different name is worse than no light theme.
        let dark = groups(Variant::Dark);
        let light = groups(Variant::Light);
        let differing = dark.iter().zip(&light).filter(|(a, b)| a.1 != b.1).count();
        assert!(differing > 10, "only {differing} groups differ between variants");
    }
}
