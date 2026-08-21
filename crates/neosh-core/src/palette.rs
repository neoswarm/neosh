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
//! - **Except in code, where seven do.** `Syntax.*` is the one group family that breaks the rule
//!   above, and it breaks it because it is not part of the same vocabulary. Every other colour here
//!   is a *state* — something is running, something needs you, something broke — and a state you
//!   have to look up is a state you will misread. The colour of a string literal is not a state; it
//!   is a convention every editor has already taught its reader, and one that no glyph or attribute
//!   can carry instead. So code gets a palette of its own, it is confined to the inside of a diff
//!   or a fence, and it never leaks out into the chrome.
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
    /// The faces of the logo, top row first: bright at the lit edge, falling away downward.
    logo: [Color; 6],
    /// The shadow the logo's letters cast.
    logo_edge: Color,
    /// The band behind a line a diff added, and behind the one it removed.
    ///
    /// Dark and desaturated on purpose. The band answers "did this line change", and the code on
    /// top of it still has to answer "what does it say" — so it is a wash rather than a colour,
    /// and every syntax hue keeps its contrast against it.
    diff_add_bg: Color,
    diff_del_bg: Color,
    /// The same, one step further, for the gutter those bands run behind.
    diff_add_gutter: Color,
    diff_del_gutter: Color,
    /// The code palette. Seven hues, which is more than the rest of this file allows itself; see
    /// the note on the rules above for why code gets to be the exception.
    code: Code,
}

/// What a token in a piece of code is coloured by.
///
/// Deliberately small: seven distinctions is what a reader can hold without a legend, and every
/// grammar in the world maps onto them. A finer set — `storage.modifier` apart from `keyword`,
/// `variable.parameter` apart from `variable` — is a distinction a highlighter can make and a
/// person reading a twelve-line diff in a chat window cannot use.
struct Code {
    comment: Color,
    keyword: Color,
    string: Color,
    number: Color,
    function: Color,
    ty: Color,
    constant: Color,
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
    logo: [
        rgb(0xf8, 0xfa, 0xfc),
        rgb(0xe2, 0xe8, 0xf0),
        rgb(0xc4, 0xcc, 0xd8),
        rgb(0xa1, 0xaa, 0xb8),
        rgb(0x7e, 0x87, 0x97),
        rgb(0x5f, 0x68, 0x77),
    ],
    logo_edge: rgb(0x3a, 0x40, 0x4c),
    diff_add_bg: rgb(0x14, 0x2b, 0x1e),
    diff_del_bg: rgb(0x38, 0x1b, 0x1c),
    diff_add_gutter: rgb(0x1d, 0x3d, 0x2a),
    diff_del_gutter: rgb(0x4c, 0x24, 0x26),
    code: Code {
        comment: rgb(0x6b, 0x74, 0x87),
        keyword: rgb(0xc6, 0x8f, 0xe6),
        string: rgb(0x98, 0xc3, 0x79),
        number: rgb(0xd1, 0x9a, 0x66),
        function: rgb(0x61, 0xaf, 0xef),
        ty: rgb(0xe5, 0xc0, 0x7b),
        constant: rgb(0x56, 0xb6, 0xc2),
    },
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
    // Reversed against a light background: the lit edge is the dark one, as a steel letter on
    // paper is darkest where it stands up.
    logo: [
        rgb(0x1e, 0x29, 0x3b),
        rgb(0x33, 0x41, 0x55),
        rgb(0x47, 0x55, 0x69),
        rgb(0x64, 0x74, 0x8b),
        rgb(0x7f, 0x8c, 0x9f),
        rgb(0x9a, 0xa5, 0xb5),
    ],
    logo_edge: rgb(0xc3, 0xc9, 0xd2),
    diff_add_bg: rgb(0xda, 0xfb, 0xe1),
    diff_del_bg: rgb(0xff, 0xeb, 0xe9),
    diff_add_gutter: rgb(0xac, 0xee, 0xbb),
    diff_del_gutter: rgb(0xff, 0xce, 0xcb),
    code: Code {
        comment: rgb(0x6a, 0x73, 0x7d),
        keyword: rgb(0xa6, 0x26, 0xa4),
        string: rgb(0x0a, 0x30, 0x69),
        number: rgb(0x98, 0x68, 0x01),
        function: rgb(0x82, 0x50, 0xdf),
        ty: rgb(0xc1, 0x84, 0x01),
        constant: rgb(0x05, 0x50, 0xae),
    },
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
        // The dot beside a tool call, which is the only place in the transcript that says whether
        // something is still happening. Pulsing while it runs and settling into a colour when it is
        // over: a call that stopped moving is a call you can stop watching.
        ("Agent.ToolRunning", spec(pulse(fg(r.attention), 2600))),
        ("Agent.ToolDone", spec(fg(r.success))),
        ("Agent.ToolFailed", spec(bold(fg(r.danger)))),
        // The subject of a call that has not come back: the command, the path, the query. A sweep
        // rather than the dot's pulse, because this is a *run* of text — a band travelling along
        // the command you are waiting on is legible from across the room, and a single glyph
        // brightening and dimming is not. It settles into the quiet colour the moment it lands.
        ("Agent.ToolLive", spec(shimmer(fg(r.active), 2000))),
        // What sort of thing a call is, worn by the tool's own name. Colour rather than a second
        // vocabulary: the name stays whatever the tool calls itself, and the column becomes
        // scannable without the transcript describing tools that do not exist.
        ("Agent.ToolRead", spec(fg(r.active))),
        ("Agent.ToolEdit", spec(fg(r.success))),
        ("Agent.ToolRun", spec(fg(r.attention))),
        ("Agent.ToolNet", spec(fg(r.accent))),
        // The agent's own checklist. Three states and three weights, so which line is being worked
        // on is answerable at a glance rather than by reading all of them: what is done recedes,
        // what is next is ordinary text, and the one in hand sweeps like a running call — it *is*
        // a running call, from the plan's point of view.
        ("Agent.PlanDone", spec(dim(fg(r.success)))),
        ("Agent.PlanActive", spec(shimmer(fg(r.active), 2000))),
        ("Agent.PlanTodo", spec(fg(r.muted))),
        // A sub-agent, while it is out. Same sweep as a running call for the same reason, and the
        // quiet colour once it is not this turn's business any more.
        ("Agent.TaskLive", spec(shimmer(fg(r.accent), 2000))),
        ("Agent.TaskIdle", spec(dim(fg(r.muted)))),
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
        // Two families, and the split matters. `Diff.Add` and `Diff.Delete` are *foreground*: they
        // colour the `+5 -1` on a card header and in a turn's summary, where the number is the
        // whole message. `Diff.AddLine` and `Diff.DelLine` are the *band* behind a changed line of
        // code, and they are background only — because the foreground of that line is already
        // spoken for by what the code says. A green `+` and a green band say the same thing twice;
        // a green band and a syntax-coloured line say two different things at once.
        ("Diff.Add", spec(fg(r.success))),
        ("Diff.Delete", spec(fg(r.danger))),
        ("Diff.AddLine", spec(bg(r.diff_add_bg))),
        ("Diff.DelLine", spec(bg(r.diff_del_bg))),
        // The `+` and `-` themselves, which stay saturated so the two kinds of line are told apart
        // without relying on colour at all — and so a 16-colour terminal that flattens both bands
        // to nothing still has the sign.
        ("Diff.SignAdd", spec(bold(fg(r.success)))),
        ("Diff.SignDel", spec(bold(fg(r.danger)))),
        // The line-number column. One step further into the hue than the band it runs beside, so
        // the numbers separate from the code without becoming a third thing to read.
        ("Diff.Gutter", spec(dim(fg(r.faint)))),
        ("Diff.GutterAdd", spec(HighlightSpec {
            fg: Some(r.muted),
            bg: Some(r.diff_add_gutter),
            ..HighlightSpec::default()
        })),
        ("Diff.GutterDel", spec(HighlightSpec {
            fg: Some(r.muted),
            bg: Some(r.diff_del_gutter),
            ..HighlightSpec::default()
        })),
        ("Diff.Header", spec(bold(fg(r.accent)))),
        ("Diff.Hunk", spec(dim(fg(r.active)))),
        // ---- code ---------------------------------------------------------
        // The one place seven hues are allowed; see the note on the rules at the top of this file.
        // `Syntax.Text` exists so a highlighter always has something to name, and so a theme can
        // paint plain code differently from plain prose without either knowing about the other.
        ("Syntax.Text", spec(fg(r.fg))),
        ("Syntax.Comment", spec(italic(fg(r.code.comment)))),
        ("Syntax.Keyword", spec(fg(r.code.keyword))),
        ("Syntax.String", spec(fg(r.code.string))),
        ("Syntax.Number", spec(fg(r.code.number))),
        ("Syntax.Function", spec(fg(r.code.function))),
        ("Syntax.Type", spec(fg(r.code.ty))),
        ("Syntax.Constant", spec(fg(r.code.constant))),
        ("Syntax.Punct", spec(fg(r.muted))),
        // ---- meters -------------------------------------------------------
        ("Meter.Fill", spec(fg(r.active))),
        ("Meter.Empty", spec(dim(fg(r.faint)))),
        // A context window nearly full is the one number that changes what you do next.
        ("Meter.Warn", spec(fg(r.attention))),
        ("Meter.Full", spec(bold(fg(r.danger)))),
        // ---- the logo -----------------------------------------------------
        // The word on the welcome screen, lit like brushed metal: a grey ramp down the faces of
        // the letters and one dark grey for the shadow under them. Greys and not the accent, so
        // the one coloured thing on that screen is still the key that does something. Six faces
        // rather than a gradient the frontend computes, because a theme should be able to repaint
        // the word — in its own metal, or flat — without the frontend knowing it is a word.
        ("Logo.Face0", spec(fg(r.logo[0]))),
        ("Logo.Face1", spec(fg(r.logo[1]))),
        ("Logo.Face2", spec(fg(r.logo[2]))),
        ("Logo.Face3", spec(fg(r.logo[3]))),
        ("Logo.Face4", spec(fg(r.logo[4]))),
        ("Logo.Face5", spec(fg(r.logo[5]))),
        ("Logo.Edge", spec(fg(r.logo_edge))),
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
    fn every_group_the_transcript_asks_for_is_defined() {
        // A mark whose group does not exist is not an error anywhere: it is drawn in the default
        // colour, which is exactly what a card looks like when it is working. These are the names
        // `neosh/src/cards.rs` writes, and the only way to notice a typo in one is to say so here.
        let names: BTreeSet<_> = contract().into_iter().collect();
        for required in [
            "Agent.ToolRunning",
            "Agent.ToolDone",
            "Agent.ToolFailed",
            "Agent.ToolLive",
            "Agent.ToolRead",
            "Agent.ToolEdit",
            "Agent.ToolRun",
            "Agent.ToolNet",
            "Agent.Tool",
            "Agent.Usage",
            "Agent.PlanDone",
            "Agent.PlanActive",
            "Agent.PlanTodo",
            "Agent.TaskLive",
            "Agent.TaskIdle",
            "Diff.Add",
            "Diff.Delete",
            "Diff.AddLine",
            "Diff.DelLine",
            "Diff.SignAdd",
            "Diff.SignDel",
            "Diff.Gutter",
            "Diff.GutterAdd",
            "Diff.GutterDel",
        ] {
            assert!(names.contains(required), "{required} is missing from the palette");
        }
    }

    #[test]
    fn the_band_behind_a_changed_line_is_a_background_and_nothing_else() {
        // The whole point of the split: a `Diff.AddLine` that also set a foreground would override
        // the syntax colour of every token on the line it sits under, and the diff would go back
        // to being two colours of text. Checked here rather than trusted, because the failure is
        // silent — the code is still readable, it is just no longer highlighted.
        for variant in [Variant::Dark, Variant::Light] {
            for name in ["Diff.AddLine", "Diff.DelLine"] {
                let def = groups(variant).into_iter().find(|(n, _)| *n == name).map(|(_, d)| d);
                let Some(HighlightDef::Spec { spec }) = def else {
                    panic!("{variant:?}: {name} is not a spec");
                };
                assert!(spec.bg.is_some(), "{variant:?}: {name} has no band");
                assert!(spec.fg.is_none(), "{variant:?}: {name} sets a foreground");
            }
        }
    }

    #[test]
    fn code_has_a_colour_for_every_token_a_highlighter_can_produce() {
        // `neosh-syntax` maps every scope it sees onto one of these. A name here that the palette
        // does not define is a token class that silently renders as plain text.
        let names: BTreeSet<_> = contract().into_iter().collect();
        for required in [
            "Syntax.Text",
            "Syntax.Comment",
            "Syntax.Keyword",
            "Syntax.String",
            "Syntax.Number",
            "Syntax.Function",
            "Syntax.Type",
            "Syntax.Constant",
            "Syntax.Punct",
        ] {
            assert!(names.contains(required), "{required} is missing from the palette");
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
