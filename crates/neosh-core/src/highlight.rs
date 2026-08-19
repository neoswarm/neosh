//! Semantic highlight groups.
//!
//! Plugins declare *names* (`AgentToolError`), never colors. A group either carries a spec or links
//! to another group. Linking is what makes a plugin look right under a theme its author never saw:
//! `MyPlugin.Border -> Float.Border` inherits whatever the active theme decided borders are.
//!
//! Down-conversion (truecolor -> 256 -> 16) happens at the terminal boundary, not here, so a plugin
//! never reasons about terminal capability and a non-terminal frontend is free to ignore the
//! question entirely.

use std::collections::{HashMap, HashSet};

use neosh_proto::{HighlightDef, HighlightSpec};

use crate::palette::{self, Variant};

#[derive(Debug, Default, Clone)]
pub struct HighlightRegistry {
    defs: HashMap<String, HighlightDef>,
    /// Which theme the defaults came from, so switching can replace exactly those and leave a
    /// plugin's own definitions alone.
    variant: Variant,
    /// Groups defined by someone other than the theme. A theme switch must not silently discard a
    /// plugin's colours, and it must not resurrect a default the plugin deliberately overrode.
    overridden: HashSet<String>,
}

/// How deep a link chain may go before we call it a cycle.
const MAX_LINK_DEPTH: usize = 16;

impl HighlightRegistry {
    pub fn new() -> Self {
        Self::with_variant(Variant::default())
    }

    pub fn with_variant(variant: Variant) -> Self {
        let mut r = Self { defs: HashMap::new(), variant, overridden: HashSet::new() };
        r.install_theme(variant);
        r
    }

    pub fn variant(&self) -> Variant {
        self.variant
    }

    fn install_theme(&mut self, variant: Variant) {
        for (name, def) in palette::groups(variant) {
            // A group a plugin defined for itself wins. Otherwise switching theme would wipe out
            // every colour a plugin chose, which is not what "switch theme" means.
            if self.overridden.contains(name) {
                continue;
            }
            self.defs.insert(name.to_string(), def);
        }
        self.variant = variant;
    }

    /// Swap the theme, keeping anything a plugin defined.
    ///
    /// Returns whether anything changed, so a caller can skip the redraw when it did not.
    pub fn set_variant(&mut self, variant: Variant) -> bool {
        if self.variant == variant {
            return false;
        }
        self.install_theme(variant);
        true
    }

    pub fn define(&mut self, name: impl Into<String>, def: HighlightDef) {
        let name = name.into();
        self.overridden.insert(name.clone());
        self.defs.insert(name, def);
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
        r.define("A", HighlightDef::Link { to: "B".into() });
        r.define("B", HighlightDef::Link { to: "Float.Border".into() });
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
        r.define("Comment", mine.clone());
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
        r.define("MyPlugin.Border", HighlightDef::Link { to: "Float.Border".into() });
        assert!(r.resolve("MyPlugin.Border").is_some());
    }

    #[test]
    fn cycles_terminate_instead_of_hanging() {
        let mut r = HighlightRegistry::new();
        r.define("A", HighlightDef::Link { to: "B".into() });
        r.define("B", HighlightDef::Link { to: "A".into() });
        assert_eq!(r.resolve("A"), None);
    }

    #[test]
    fn unknown_group_resolves_to_none_rather_than_a_default() {
        let r = HighlightRegistry::new();
        assert_eq!(r.resolve("Nope.Nope"), None);
    }
}
