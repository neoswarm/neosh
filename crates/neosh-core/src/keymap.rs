//! Keymaps: Neovim-style notation, multi-key sequences, and scoped resolution.
//!
//! Two properties matter here.
//!
//! **Routing is decided in the core, synchronously.** A plugin binds a key to a *command name*,
//! never to an opaque callback, so the core can answer "who owns this key" from its own table
//! without a round trip into a plugin runtime. Only the command body runs asynchronously. It also
//! means every binding is listable and remappable by the user, which a callback-based API cannot
//! offer.
//!
//! **Scope order is fixed**: focused window, then that window's buffer, then global. First match
//! wins. This is what stops plugins fighting over `<Esc>`.

use std::collections::HashMap;

use neosh_proto::{ApiError, KeyCode, KeyMods, KeyPress, KeymapScope, Mode};

/// A parsed left-hand side: one or more key presses, e.g. `gd` or `<C-w>v`.
pub type KeySeq = Vec<KeyPress>;

/// Substitute `<leader>` for whatever the leader key is set to.
///
/// Done at *map* time, not at press time — the same as Neovim, and for the same reason: a binding
/// that silently moved when you changed `mapleader` halfway through your config would be impossible
/// to reason about. Change the leader before you use it.
///
/// The match is case-insensitive because `<Leader>` is how most people write it.
pub fn expand_leader(lhs: &str, leader: &str) -> String {
    let mut out = String::with_capacity(lhs.len());
    let mut rest = lhs;
    while let Some(at) = rest.to_ascii_lowercase().find("<leader>") {
        out.push_str(&rest[..at]);
        out.push_str(leader);
        rest = &rest[at + "<leader>".len()..];
    }
    out.push_str(rest);
    out
}

/// Parse Neovim-style key notation.
///
/// Accepts `<C-x>`, `<S-Tab>`, `<A-f>`/`<M-f>`, `<D-s>`, named keys such as `<Esc>` `<CR>` `<Tab>`
/// `<Space>` `<BS>` `<F5>`, `<lt>` for a literal `<`, and bare characters which form a sequence:
/// `gd` is two presses.
pub fn parse_keys(lhs: &str) -> Result<KeySeq, ApiError> {
    let mut out = Vec::new();
    let chars: Vec<char> = lhs.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            let close = chars[i + 1..]
                .iter()
                .position(|c| *c == '>')
                .map(|p| p + i + 1)
                .ok_or_else(|| ApiError::InvalidArgument {
                    message: format!("unterminated '<' in key notation {lhs:?}"),
                })?;
            let inner: String = chars[i + 1..close].iter().collect();
            out.push(parse_bracketed(&inner, lhs)?);
            i = close + 1;
        } else {
            out.push(KeyPress {
                code: KeyCode::Char { c: chars[i].to_string() },
                mods: KeyMods::default(),
            });
            i += 1;
        }
    }
    if out.is_empty() {
        return Err(ApiError::InvalidArgument { message: "empty key notation".into() });
    }
    Ok(out)
}

fn parse_bracketed(inner: &str, whole: &str) -> Result<KeyPress, ApiError> {
    let mut mods = KeyMods::default();
    let mut rest = inner;

    // Modifier prefixes may stack: <C-S-x>.
    loop {
        let (flag, tail) = match rest.get(..2).map(str::to_ascii_uppercase).as_deref() {
            Some("C-") => (Some(&mut mods.ctrl), &rest[2..]),
            Some("S-") => (Some(&mut mods.shift), &rest[2..]),
            Some("A-") | Some("M-") => (Some(&mut mods.alt), &rest[2..]),
            Some("D-") => (Some(&mut mods.meta), &rest[2..]),
            _ => (None, rest),
        };
        match flag {
            Some(f) => {
                *f = true;
                rest = tail;
            }
            None => break,
        }
    }

    let code = match rest.to_ascii_lowercase().as_str() {
        "esc" => KeyCode::Esc,
        "cr" | "enter" | "return" => KeyCode::Enter,
        // A terminal has no such thing as tab-with-shift held: it sends a different sequence
        // entirely, which arrives as `BackTab` with no modifier. Everyone writes the binding as
        // `<S-Tab>`, so that notation has to land on the key the terminal will actually deliver —
        // otherwise the binding is accepted, listed, shown in the footer, and never fires.
        "tab" if mods.shift => {
            mods.shift = false;
            KeyCode::BackTab
        }
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "bs" | "backspace" => KeyCode::Backspace,
        "del" | "delete" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "space" => KeyCode::Char { c: " ".into() },
        "lt" => KeyCode::Char { c: "<".into() },
        "gt" => KeyCode::Char { c: ">".into() },
        "bslash" => KeyCode::Char { c: "\\".into() },
        "bar" => KeyCode::Char { c: "|".into() },
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        f if f.starts_with('f') && f[1..].parse::<u8>().is_ok() => {
            KeyCode::F { n: f[1..].parse().expect("checked above") }
        }
        _ => {
            let mut it = rest.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => KeyCode::Char { c: c.to_string() },
                _ => {
                    return Err(ApiError::InvalidArgument {
                        message: format!("unknown key {inner:?} in {whole:?}"),
                    });
                }
            }
        }
    };
    Ok(KeyPress { code, mods })
}

/// Render a key sequence back to notation. Used for `:map`-style listings and hint floats.
pub fn format_keys(seq: &[KeyPress]) -> String {
    seq.iter().map(format_key).collect()
}

fn format_key(k: &KeyPress) -> String {
    let named = |s: &str| s.to_string();
    let base = match &k.code {
        KeyCode::Char { c } if c == " " => named("Space"),
        KeyCode::Char { c } if c == "<" => named("lt"),
        KeyCode::Char { c } => c.clone(),
        KeyCode::Enter => named("CR"),
        KeyCode::Tab => named("Tab"),
        KeyCode::BackTab => named("S-Tab"),
        KeyCode::Backspace => named("BS"),
        KeyCode::Delete => named("Del"),
        KeyCode::Insert => named("Insert"),
        KeyCode::Esc => named("Esc"),
        KeyCode::Left => named("Left"),
        KeyCode::Right => named("Right"),
        KeyCode::Up => named("Up"),
        KeyCode::Down => named("Down"),
        KeyCode::Home => named("Home"),
        KeyCode::End => named("End"),
        KeyCode::PageUp => named("PageUp"),
        KeyCode::PageDown => named("PageDown"),
        KeyCode::F { n } => format!("F{n}"),
    };
    let plain_char = matches!(&k.code, KeyCode::Char { c } if c != " " && c != "<");
    if k.mods == KeyMods::default() && plain_char {
        return base;
    }
    let mut s = String::from("<");
    if k.mods.ctrl {
        s.push_str("C-");
    }
    if k.mods.alt {
        s.push_str("A-");
    }
    if k.mods.shift {
        s.push_str("S-");
    }
    if k.mods.meta {
        s.push_str("D-");
    }
    s.push_str(&base);
    s.push('>');
    s
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub command: String,
    pub desc: Option<String>,
    /// Which plugin owns this binding, so unloading a plugin can remove its maps.
    pub owner: Option<String>,
}

/// What happened when a key was fed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyResolution {
    /// Nothing is bound and nothing could be. The caller should treat the key as unhandled — for
    /// `<Esc>` that means "interrupt the agent".
    Unhandled,
    /// The sequence so far is a prefix of at least one binding. Hold it and wait for more.
    Pending,
    Matched { command: String, scope: KeymapScope },
}

type Key = (Mode, KeymapScope, KeySeq);

#[derive(Debug, Default, Clone)]
pub struct KeymapTable {
    maps: HashMap<Key, Binding>,
}

impl KeymapTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a key, unless a bundled plugin is trying to take one somebody else already claimed.
    ///
    /// `bundled` is the set of plugins that ship with neosh. Their bindings are *defaults*, and a
    /// default that overwrites a choice is not a default. This matters because `init.ts` runs
    /// **before** plugin discovery — it decides what else loads — so without this rule every
    /// bundled plugin silently steals whichever key the user's configuration had just bound.
    ///
    /// Returns whether the binding was made, so the caller can say so rather than leaving a
    /// keystroke that quietly does something else.
    pub fn set(
        &mut self,
        mode: Mode,
        scope: KeymapScope,
        lhs: &str,
        binding: Binding,
        bundled: &std::collections::HashSet<String>,
    ) -> Result<bool, ApiError> {
        let seq = parse_keys(lhs)?;
        let key = (mode, scope, seq);
        let new_is_bundled = binding.owner.as_ref().is_some_and(|o| bundled.contains(o));
        if new_is_bundled
            && let Some(existing) = self.maps.get(&key)
            && existing.owner.as_ref().is_some_and(|o| !bundled.contains(o))
        {
            return Ok(false);
        }
        self.maps.insert(key, binding);
        Ok(true)
    }

    pub fn del(&mut self, mode: Mode, scope: KeymapScope, lhs: &str) -> Result<(), ApiError> {
        let seq = parse_keys(lhs)?;
        self.maps.remove(&(mode, scope, seq));
        Ok(())
    }

    /// Drop every binding owned by a plugin. Called on unload so a removed plugin cannot keep
    /// squatting on keys.
    /// Drop every binding scoped to a window that no longer exists.
    ///
    /// Window-scoped bindings are how a widget takes ownership of a key for as long as it is on
    /// screen. Without this they outlive the widget: `keymap.list` grows by a dozen entries every
    /// time a picker opens, and the help screen fills with keys belonging to windows that are gone.
    pub fn remove_window(&mut self, win: neosh_proto::WindowId) {
        self.maps.retain(|(_, scope, _), _| *scope != KeymapScope::Window { win });
    }

    pub fn remove_owner(&mut self, owner: &str) {
        self.maps.retain(|_, b| b.owner.as_deref() != Some(owner));
    }

    pub fn list(&self, mode: Option<Mode>) -> Vec<(Mode, KeymapScope, KeySeq, Binding)> {
        let mut v: Vec<_> = self
            .maps
            .iter()
            .filter(|((m, _, _), _)| mode.is_none_or(|want| *m == want))
            .map(|((m, s, k), b)| (*m, *s, k.clone(), b.clone()))
            .collect();
        v.sort_by(|a, b| format_keys(&a.2).cmp(&format_keys(&b.2)));
        v
    }

    /// Resolve a pending sequence.
    ///
    /// `scopes` must be supplied most-specific-first — focused window, then its buffer, then
    /// global. An exact match in a nearer scope beats a longer possible match in a farther one.
    pub fn resolve(&self, mode: Mode, scopes: &[KeymapScope], seq: &[KeyPress]) -> KeyResolution {
        for scope in scopes {
            if let Some(b) = self.maps.get(&(mode, *scope, seq.to_vec())) {
                return KeyResolution::Matched { command: b.command.clone(), scope: *scope };
            }
        }
        let could_extend = self.maps.keys().any(|(m, s, k)| {
            *m == mode && scopes.contains(s) && k.len() > seq.len() && k.starts_with(seq)
        });
        if could_extend { KeyResolution::Pending } else { KeyResolution::Unhandled }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No plugin is bundled, so every binding simply takes the key. The rule that bundled defaults
    /// defer to somebody else's choice is tested in `neosh` end to end, where there is a real
    /// `init.ts` and real bundled plugins to be in the wrong order about.
    fn none() -> std::collections::HashSet<String> {
        std::collections::HashSet::new()
    }

    #[test]
    fn leader_is_substituted_wherever_it_appears() {
        assert_eq!(expand_leader("<leader>pa", "\\"), "\\pa");
        assert_eq!(expand_leader("<Leader>x", " "), " x");
        // Written twice on purpose: `<leader><leader>` is a common "the other one" binding.
        assert_eq!(expand_leader("<leader><leader>", ","), ",,");
    }

    #[test]
    fn a_binding_with_no_leader_is_left_exactly_as_it_was() {
        assert_eq!(expand_leader("<C-o>", "\\"), "<C-o>");
        assert_eq!(expand_leader("gd", ","), "gd");
    }

    #[test]
    fn a_leader_that_is_itself_notation_still_parses() {
        // `<leader>` set to `<Space>` has to survive being spliced into the middle of an lhs and
        // then parsed — the substitution is textual, so this is the case that would break.
        let lhs = expand_leader("<leader>pa", "<Space>");
        assert_eq!(lhs, "<Space>pa");
        let seq = parse_keys(&lhs).expect("parses");
        assert_eq!(seq.len(), 3);
        assert_eq!(seq[0].code, KeyCode::Char { c: " ".into() });
    }

    fn ch(c: &str) -> KeyPress {
        KeyPress { code: KeyCode::Char { c: c.into() }, mods: KeyMods::default() }
    }

    #[test]
    fn parses_ctrl_and_named_keys() {
        let k = parse_keys("<C-x>").unwrap();
        assert_eq!(k.len(), 1);
        assert!(k[0].mods.ctrl);
        assert_eq!(k[0].code, KeyCode::Char { c: "x".into() });

        assert_eq!(parse_keys("<Esc>").unwrap()[0].code, KeyCode::Esc);
        assert_eq!(parse_keys("<CR>").unwrap()[0].code, KeyCode::Enter);
        assert_eq!(parse_keys("<F12>").unwrap()[0].code, KeyCode::F { n: 12 });
        assert_eq!(parse_keys("<Space>").unwrap()[0].code, KeyCode::Char { c: " ".into() });
    }

    #[test]
    fn parses_stacked_modifiers() {
        let k = &parse_keys("<C-S-A-p>").unwrap()[0];
        assert!(k.mods.ctrl && k.mods.shift && k.mods.alt);
        assert!(!k.mods.meta);
    }

    #[test]
    fn alt_and_meta_are_synonyms() {
        assert_eq!(parse_keys("<A-f>").unwrap(), parse_keys("<M-f>").unwrap());
    }

    #[test]
    fn bare_characters_form_a_sequence() {
        assert_eq!(parse_keys("gd").unwrap(), vec![ch("g"), ch("d")]);
    }

    #[test]
    fn mixed_notation_and_literals() {
        let k = parse_keys("<C-w>v").unwrap();
        assert_eq!(k.len(), 2);
        assert!(k[0].mods.ctrl);
        assert_eq!(k[1].code, KeyCode::Char { c: "v".into() });
    }

    #[test]
    fn lt_escapes_a_literal_angle_bracket() {
        assert_eq!(parse_keys("<lt>").unwrap()[0].code, KeyCode::Char { c: "<".into() });
    }

    #[test]
    fn unterminated_bracket_is_an_error_not_a_panic() {
        assert!(parse_keys("<C-x").is_err());
        assert!(parse_keys("<nonsense>").is_err());
        assert!(parse_keys("").is_err());
    }

    #[test]
    fn notation_round_trips() {
        for s in ["<C-x>", "<Esc>", "<F5>", "gd", "<C-w>v", "<A-S-p>"] {
            let parsed = parse_keys(s).unwrap();
            let reparsed = parse_keys(&format_keys(&parsed)).unwrap();
            assert_eq!(parsed, reparsed, "round trip failed for {s}");
        }
    }

    fn table() -> KeymapTable {
        let mut t = KeymapTable::new();
        let b = |c: &str| Binding { command: c.into(), desc: None, owner: Some("p".into()) };
        t.set(Mode::Normal, KeymapScope::Global, "gd", b("goto.def"), &none())
        .unwrap();
        t.set(Mode::Normal, KeymapScope::Global, "<C-p>", b("picker.open"), &none())
        .unwrap();
        t
    }

    #[test]
    fn multi_key_sequence_reports_pending_then_matches() {
        let t = table();
        let scopes = [KeymapScope::Global];
        assert_eq!(t.resolve(Mode::Normal, &scopes, &[ch("g")]), KeyResolution::Pending);
        match t.resolve(Mode::Normal, &scopes, &[ch("g"), ch("d")]) {
            KeyResolution::Matched { command, .. } => assert_eq!(command, "goto.def"),
            other => panic!("expected a match, got {other:?}"),
        }
    }

    #[test]
    fn unbound_key_is_unhandled_so_esc_can_reach_the_interrupt() {
        let t = table();
        assert_eq!(
            t.resolve(Mode::Normal, &[KeymapScope::Global], &[KeyPress {
                code: KeyCode::Esc,
                mods: KeyMods::default()
            }]),
            KeyResolution::Unhandled
        );
    }

    #[test]
    fn nearer_scope_wins_over_global() {
        let mut t = table();
        let win = KeymapScope::Window { win: neosh_proto::WindowId(7) };
        t.set(Mode::Normal, win, "<C-p>", Binding {
            command: "float.close".into(),
            desc: None,
            owner: None,
        }, &none())
        .unwrap();

        let scopes = [win, KeymapScope::Global];
        let key = parse_keys("<C-p>").unwrap();
        match t.resolve(Mode::Normal, &scopes, &key) {
            KeyResolution::Matched { command, scope } => {
                assert_eq!(command, "float.close");
                assert_eq!(scope, win);
            }
            other => panic!("expected the window-scoped binding, got {other:?}"),
        }
    }

    #[test]
    fn modes_do_not_leak_into_each_other() {
        let t = table();
        assert_eq!(
            t.resolve(Mode::Insert, &[KeymapScope::Global], &parse_keys("<C-p>").unwrap()),
            KeyResolution::Unhandled
        );
    }

    #[test]
    fn unloading_a_plugin_drops_its_bindings() {
        let mut t = table();
        t.remove_owner("p");
        assert!(t.list(None).is_empty());
    }

    /// `<S-Tab>` is one key press, and a terminal delivers it as its own code with no modifier.
    ///
    /// Regression: it parsed to tab-with-shift, which is a key press no terminal produces. The
    /// binding was accepted, listed in the key sheet and shown in the footer, and never once fired.
    #[test]
    fn shift_tab_is_the_key_a_terminal_actually_sends() {
        let parsed = parse_keys("<S-Tab>").expect("parses");
        assert_eq!(parsed, vec![KeyPress { code: KeyCode::BackTab, mods: KeyMods::default() }]);
        assert_eq!(parse_keys("<BackTab>").expect("parses"), parsed);
        // And it round-trips, so the sheet says what you would have to type to bind it again.
        assert_eq!(format_keys(&parsed), "<S-Tab>");
    }

    #[test]
    fn a_plain_tab_is_still_a_plain_tab() {
        assert_eq!(
            parse_keys("<Tab>").expect("parses"),
            vec![KeyPress { code: KeyCode::Tab, mods: KeyMods::default() }]
        );
    }
}
