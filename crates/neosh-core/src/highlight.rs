//! Semantic highlight groups.
//!
//! Plugins declare *names* (`AgentToolError`), never colors. A group either carries a spec or links
//! to another group. Linking is what makes a plugin look right under a theme its author never saw:
//! `MyPlugin.Border -> Float.Border` inherits whatever the active theme decided borders are.
//!
//! Down-conversion (truecolor -> 256 -> 16) happens at the terminal boundary, not here, so a plugin
//! never reasons about terminal capability and a non-terminal frontend is free to ignore the
//! question entirely.

use std::collections::HashMap;

use neosh_proto::{HighlightDef, HighlightSpec};

use crate::palette::{self, Variant};

#[derive(Debug, Clone)]
pub struct HighlightRegistry {
    defs: HashMap<String, HighlightDef>,
    /// Which theme the defaults came from, so switching can replace exactly those and leave a
    /// plugin's own definitions alone.
    variant: Variant,
    /// A theme somebody contributed, laid over `variant`: its name, and its groups.
    ///
    /// A theme that is a plugin is a list of definitions on top of one of the two built-in
    /// variants, which is what lets it say only the groups it has an opinion about and still
    /// resolve every name in the contract. See [`Self::set_custom`].
    custom: Option<(String, Vec<(String, HighlightDef)>)>,
    /// Groups defined by someone other than the theme, and by whom. A theme switch must not
    /// silently discard a plugin's colours, and it must not resurrect a default the plugin
    /// deliberately overrode — and when the plugin goes, its groups go with it, which is the part
    /// a set without owners could not do.
    owners: HashMap<String, String>,
    /// Whether groups that animate keep their animation. Follows `ui.motion`.
    motion: bool,
}

/// What changed when a group was reset or its owner went away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restored {
    /// The theme's definition is back.
    Theme(HighlightDef),
    /// Nothing stands behind the name any more.
    Cleared,
}

/// How deep a link chain may go before we call it a cycle.
const MAX_LINK_DEPTH: usize = 16;

impl Default for HighlightRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl HighlightRegistry {
    pub fn new() -> Self {
        Self::with_variant(Variant::default())
    }

    pub fn with_variant(variant: Variant) -> Self {
        let mut r = Self {
            defs: HashMap::new(),
            variant,
            custom: None,
            owners: HashMap::new(),
            motion: true,
        };
        r.install_theme(variant);
        r
    }

    pub fn variant(&self) -> Variant {
        self.variant
    }

    /// The contributed theme in use, if any.
    pub fn custom(&self) -> Option<&str> {
        self.custom.as_ref().map(|(n, _)| n.as_str())
    }

    /// What the theme in use says a group is — the palette's definition, with the contributed
    /// theme's laid over it. `None` for a name the theme has no opinion about.
    fn theme_def(&self, name: &str) -> Option<HighlightDef> {
        let over = self
            .custom
            .as_ref()
            .and_then(|(_, groups)| groups.iter().find(|(n, _)| n == name).map(|(_, d)| d.clone()));
        let mut def = over.or_else(|| {
            palette::groups(self.variant).into_iter().find(|(n, _)| *n == name).map(|(_, d)| d)
        })?;
        if !self.motion
            && let HighlightDef::Spec { spec } = &mut def
        {
            spec.animate = None;
        }
        Some(def)
    }

    fn install_theme(&mut self, variant: Variant) {
        // Everything the previous theme defined and this one does not has to go, or a contributed
        // theme's extra groups outlive switching away from it.
        let stale: Vec<String> = self
            .defs
            .keys()
            .filter(|n| !self.owners.contains_key(*n))
            .cloned()
            .collect();
        for name in stale {
            self.defs.remove(&name);
        }
        self.variant = variant;
        let names: Vec<String> = palette::groups(variant)
            .into_iter()
            .map(|(n, _)| n.to_string())
            .chain(
                self.custom.iter().flat_map(|(_, g)| g.iter().map(|(n, _)| n.clone())),
            )
            .collect();
        for name in names {
            // A group a plugin defined for itself wins. Otherwise switching theme would wipe out
            // every colour a plugin chose, which is not what "switch theme" means.
            if self.owners.contains_key(&name) {
                continue;
            }
            if let Some(def) = self.theme_def(&name) {
                self.defs.insert(name, def);
            }
        }
    }

    /// Swap the theme, keeping anything a plugin defined.
    ///
    /// Returns whether anything changed, so a caller can skip the redraw when it did not.
    pub fn set_variant(&mut self, variant: Variant) -> bool {
        if self.variant == variant && self.custom.is_none() {
            return false;
        }
        self.custom = None;
        self.install_theme(variant);
        true
    }

    /// Use a contributed theme: `groups` laid over `base`.
    ///
    /// Returns whether anything changed. A theme that names no groups is a renamed variant, which
    /// is allowed — it is still a thing that can be listed and chosen.
    pub fn set_custom(
        &mut self,
        name: &str,
        base: Variant,
        groups: Vec<(String, HighlightDef)>,
    ) -> bool {
        if self.variant == base && self.custom.as_ref().is_some_and(|(n, g)| n == name && *g == groups)
        {
            return false;
        }
        self.custom = Some((name.to_string(), groups));
        self.install_theme(base);
        true
    }

    /// Turn motion on or off across every group that has any.
    ///
    /// Done by *removing* the animation from the definitions rather than by telling the frontend
    /// to ignore it. That keeps `ui.motion` a single decision made in one place, and it means a
    /// frontend needs no concept of the setting at all — it animates what carries an animation,
    /// and with motion off nothing does. A group keeps its colours either way: turning movement off
    /// must not turn text invisible.
    ///
    /// Returns whether anything changed.
    pub fn set_motion(&mut self, on: bool) -> bool {
        if self.motion == on {
            return false;
        }
        self.motion = on;
        let names: Vec<String> = self.defs.keys().cloned().collect();
        let mut changed = false;
        for name in names {
            // A plugin that redefined the group owns it; restoring the catalogue's version here
            // would undo their choice as a side effect of a setting they did not touch.
            if self.owners.contains_key(name.as_str()) {
                continue;
            }
            let Some(def) = self.theme_def(&name) else { continue };
            if self.defs.get(&name) == Some(&def) {
                continue;
            }
            self.defs.insert(name, def);
            changed = true;
        }
        changed
    }

    /// Define a group on behalf of `owner`. Returns whether anything changed.
    ///
    /// With `default`, only if nobody has defined it — neither the theme nor another plugin —
    /// which is the call a plugin makes for the groups it introduces, so that a user's `init.ts`
    /// wins without having to load after it. Without, the caller wins over everything, the theme
    /// included; a second plugin redefining a group another plugin owns takes it over, and the
    /// registry reports the previous owner so the host can say so.
    pub fn define(
        &mut self,
        name: impl Into<String>,
        def: HighlightDef,
        owner: &str,
        default: bool,
    ) -> bool {
        let name = name.into();
        if default && self.defs.contains_key(&name) {
            return false;
        }
        if self.defs.get(&name) == Some(&def) && self.owners.get(&name).map(String::as_str) == Some(owner) {
            return false;
        }
        self.owners.insert(name.clone(), owner.to_string());
        self.defs.insert(name, def);
        true
    }

    /// Who defined a group, if a plugin did.
    pub fn owner(&self, name: &str) -> Option<&str> {
        self.owners.get(name).map(String::as_str)
    }

    /// Undo a plugin's definition. `None` when no plugin had defined it.
    pub fn reset(&mut self, name: &str) -> Option<Restored> {
        self.owners.remove(name)?;
        Some(match self.theme_def(name) {
            Some(def) => {
                self.defs.insert(name.to_string(), def.clone());
                Restored::Theme(def)
            }
            None => {
                self.defs.remove(name);
                Restored::Cleared
            }
        })
    }

    /// Undo everything a plugin defined, reporting each group and what it went back to.
    pub fn remove_owner(&mut self, owner: &str) -> Vec<(String, Restored)> {
        let mut mine: Vec<String> = self
            .owners
            .iter()
            .filter(|(_, o)| o.as_str() == owner)
            .map(|(n, _)| n.clone())
            .collect();
        mine.sort();
        mine.into_iter()
            .filter_map(|name| self.reset(&name).map(|r| (name, r)))
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&HighlightDef> {
        self.defs.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &HighlightDef)> {
        self.defs.iter()
    }

    /// Follow a link chain to a concrete spec.
    ///
    /// Returns `None` for an unknown group or a cycle. A cycle is a plugin bug, but it must not
    /// hang the renderer, so the depth limit is a hard stop rather than a `HashSet` walk.
    pub fn resolve(&self, name: &str) -> Option<HighlightSpec> {
        let mut cur = name;
        for _ in 0..MAX_LINK_DEPTH {
            match self.defs.get(cur)? {
                HighlightDef::Spec { spec } => return Some(*spec),
                HighlightDef::Link { to } => cur = to,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_resolves_through_a_chain() {
        let mut r = HighlightRegistry::new();
        r.define("A", HighlightDef::Link { to: "B".into() }, "p", false);
        r.define("B", HighlightDef::Link { to: "Float.Border".into() }, "p", false);
        let spec = r.resolve("A").expect("chain should resolve");
        assert_eq!(spec, r.resolve("Float.Border").expect("the theme defines it"));
        assert!(spec.fg.is_some(), "and it resolves to a real colour, not a plain default");
    }

    #[test]
    fn every_group_a_plugin_may_link_to_resolves() {
        // The contract this registry exists to keep. A dead link does not error — it renders plain,
        // which is how an invisible selection highlight ships without anyone noticing.
        for variant in [Variant::Dark, Variant::Light] {
            let r = HighlightRegistry::with_variant(variant);
            for name in crate::palette::contract() {
                assert!(r.resolve(name).is_some(), "{variant:?}: {name} does not resolve");
            }
        }
    }

    #[test]
    fn switching_theme_keeps_what_a_plugin_defined() {
        let mut r = HighlightRegistry::with_variant(Variant::Dark);
        let mine = HighlightDef::Spec {
            spec: HighlightSpec {
                fg: Some(neosh_proto::Color::Rgb { r: 1, g: 2, b: 3 }),
                ..HighlightSpec::default()
            },
        };
        r.define("Comment", mine.clone(), "p", false);
        assert!(r.set_variant(Variant::Light));
        assert_eq!(
            r.get("Comment"),
            Some(&mine),
            "a theme switch must not throw away a colour a plugin chose"
        );
        // But groups the plugin did not touch do change.
        assert_ne!(
            r.resolve("Status.Working"),
            HighlightRegistry::with_variant(Variant::Dark).resolve("Status.Working")
        );
    }

    #[test]
    fn switching_to_the_theme_already_in_use_changes_nothing() {
        let mut r = HighlightRegistry::with_variant(Variant::Dark);
        assert!(!r.set_variant(Variant::Dark));
    }

    #[test]
    fn plugin_group_linking_to_builtin_works_with_no_theme() {
        let mut r = HighlightRegistry::new();
        r.define("MyPlugin.Border", HighlightDef::Link { to: "Float.Border".into() }, "p", false);
        assert!(r.resolve("MyPlugin.Border").is_some());
    }

    #[test]
    fn cycles_terminate_instead_of_hanging() {
        let mut r = HighlightRegistry::new();
        r.define("A", HighlightDef::Link { to: "B".into() }, "p", false);
        r.define("B", HighlightDef::Link { to: "A".into() }, "p", false);
        assert_eq!(r.resolve("A"), None);
    }

    #[test]
    fn unknown_group_resolves_to_none_rather_than_a_default() {
        let r = HighlightRegistry::new();
        assert_eq!(r.resolve("Nope.Nope"), None);
    }

    fn red() -> HighlightDef {
        HighlightDef::Spec {
            spec: HighlightSpec {
                fg: Some(neosh_proto::Color::Rgb { r: 200, g: 0, b: 0 }),
                ..HighlightSpec::default()
            },
        }
    }

    #[test]
    fn a_default_definition_yields_to_whoever_was_first() {
        let mut r = HighlightRegistry::new();
        // The theme already defines `Comment`: a default does nothing to it.
        assert!(!r.define("Comment", red(), "p", true));
        assert_eq!(r.owner("Comment"), None);
        // A new group: the first default takes it, the second does not.
        assert!(r.define("Acme.Row", HighlightDef::Link { to: "Normal".into() }, "user", true));
        assert!(!r.define("Acme.Row", red(), "acme", true));
        assert_eq!(r.owner("Acme.Row"), Some("user"));
        // A non-default definition still wins over a default one.
        assert!(r.define("Acme.Row", red(), "acme", false));
        assert_eq!(r.owner("Acme.Row"), Some("acme"));
    }

    #[test]
    fn unloading_a_plugin_takes_its_groups_back_to_the_theme_or_away() {
        let mut r = HighlightRegistry::new();
        let theme_comment = r.get("Comment").cloned().expect("theme defines it");
        r.define("Comment", red(), "acme", false);
        r.define("Acme.Row", red(), "acme", false);
        r.define("Other.Row", red(), "other", false);
        let gone = r.remove_owner("acme");
        assert_eq!(gone, vec![
            ("Acme.Row".to_string(), Restored::Cleared),
            ("Comment".to_string(), Restored::Theme(theme_comment.clone())),
        ]);
        assert_eq!(r.get("Comment"), Some(&theme_comment));
        assert_eq!(r.get("Acme.Row"), None);
        assert_eq!(r.get("Other.Row"), Some(&red()), "somebody else's stays");
    }

    #[test]
    fn a_reset_group_follows_the_theme_again() {
        let mut r = HighlightRegistry::with_variant(Variant::Dark);
        r.define("Comment", red(), "acme", false);
        assert!(r.set_variant(Variant::Light));
        assert_eq!(r.get("Comment"), Some(&red()), "pinned while owned");
        assert!(matches!(r.reset("Comment"), Some(Restored::Theme(_))));
        assert_eq!(
            r.get("Comment"),
            HighlightRegistry::with_variant(Variant::Light).get("Comment"),
            "and back on the theme in use, not the one it was overridden under"
        );
        assert_eq!(r.reset("Comment"), None, "nothing left to reset");
    }

    #[test]
    fn a_contributed_theme_lays_over_its_base_and_leaves_with_a_switch() {
        let mut r = HighlightRegistry::with_variant(Variant::Dark);
        let groups = vec![
            ("Comment".to_string(), red()),
            ("Gruv.Extra".to_string(), HighlightDef::Link { to: "Normal".into() }),
        ];
        assert!(r.set_custom("gruvbox", Variant::Light, groups.clone()));
        assert_eq!(r.custom(), Some("gruvbox"));
        assert_eq!(r.get("Comment"), Some(&red()));
        assert!(r.resolve("Gruv.Extra").is_some());
        // Everything it did not name is the base's.
        assert_eq!(
            r.resolve("Status.Working"),
            HighlightRegistry::with_variant(Variant::Light).resolve("Status.Working")
        );
        // A plugin's own definition still wins over the theme's.
        let blue = HighlightDef::Link { to: "Accent".into() };
        r.define("Comment", blue.clone(), "acme", false);
        assert!(!r.set_custom("gruvbox", Variant::Light, groups.clone()), "same theme, no change");
        assert_eq!(r.get("Comment"), Some(&blue));
        // Switching back to a built-in variant drops the theme's extra groups.
        assert!(r.set_variant(Variant::Dark));
        assert_eq!(r.custom(), None);
        assert_eq!(r.get("Gruv.Extra"), None);
        assert_eq!(r.get("Comment"), Some(&blue), "and keeps the plugin's");
    }
}
