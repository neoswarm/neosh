//! Which plugin loads before which.
//!
//! Load order used to be the filesystem's: bundled plugins in directory order, then each plugin
//! directory sorted by name, and the code leaned on it — "a user plugin wins by loading after". That
//! stays the default. What this adds is a way to *say* it: `requires` names a plugin this one cannot
//! activate without, `after` names one it would rather follow if present, and the order here is the
//! default order bent as little as possible to honour both.

use std::collections::{HashMap, HashSet};

/// One plugin's position in the default order, and what it asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub name: String,
    pub requires: Vec<String>,
    pub after: Vec<String>,
}

/// A plugin that cannot be loaded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Names something that is not being loaded — not installed, disabled, or itself refused.
    MissingRequirement { plugin: String, requires: String },
    /// Part of a `requires`/`after` cycle, which has no order that satisfies it.
    Cycle { plugin: String },
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRequirement { plugin, requires } => write!(
                f,
                "plugin {plugin} requires {requires:?}, which is not loaded — is it installed, \
                 and not in `plugins.disabled`?"
            ),
            Self::Cycle { plugin } => write!(
                f,
                "plugin {plugin} is in a `requires`/`after` cycle and cannot be ordered"
            ),
        }
    }
}

/// Order `candidates` so that everything a plugin `requires` or wants to be `after` comes first.
///
/// Returns the indices into `candidates` in load order, and the refusals. A plugin whose
/// requirement is missing is dropped, and so is anything that required *it* — transitively, until
/// nothing else changes — because loading a dependent whose dependency is gone is how a plugin
/// fails three calls in with `undefined`. `after` naming an absent plugin is simply satisfied.
///
/// Among plugins with no constraint between them the input order is kept, so a workspace without
/// any `requires` loads exactly as it always did.
pub fn order(candidates: &[Candidate]) -> (Vec<usize>, Vec<Refused>) {
    let mut refused = Vec::new();
    let mut present: HashSet<&str> = candidates.iter().map(|c| c.name.as_str()).collect();

    // Drop anything with a missing requirement, and keep dropping until it settles.
    loop {
        let mut dropped = false;
        for c in candidates {
            if !present.contains(c.name.as_str()) {
                continue;
            }
            if let Some(missing) = c.requires.iter().find(|r| !present.contains(r.as_str())) {
                refused.push(Refused::MissingRequirement {
                    plugin: c.name.clone(),
                    requires: missing.clone(),
                });
                present.remove(c.name.as_str());
                dropped = true;
            }
        }
        if !dropped {
            break;
        }
    }

    // Kahn's algorithm over the survivors, always taking the earliest ready candidate so the
    // default order is preserved wherever the constraints allow it.
    let index: HashMap<&str, usize> =
        candidates.iter().enumerate().map(|(i, c)| (c.name.as_str(), i)).collect();
    let mut pending: HashMap<usize, usize> = HashMap::new(); // index -> unmet predecessors
    let mut dependents: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, c) in candidates.iter().enumerate() {
        if !present.contains(c.name.as_str()) {
            continue;
        }
        let mut unmet = 0;
        for before in c.requires.iter().chain(c.after.iter()) {
            let Some(&j) = index.get(before.as_str()) else { continue };
            if !present.contains(before.as_str()) || j == i {
                continue;
            }
            unmet += 1;
            dependents.entry(j).or_default().push(i);
        }
        pending.insert(i, unmet);
    }

    let mut out = Vec::with_capacity(pending.len());
    loop {
        let Some(&next) = pending.iter().filter(|(_, n)| **n == 0).map(|(i, _)| i).min() else {
            break;
        };
        pending.remove(&next);
        out.push(next);
        for d in dependents.remove(&next).unwrap_or_default() {
            if let Some(n) = pending.get_mut(&d) {
                *n -= 1;
            }
        }
    }
    let mut stuck: Vec<usize> = pending.into_keys().collect();
    stuck.sort_unstable();
    for i in stuck {
        refused.push(Refused::Cycle { plugin: candidates[i].name.clone() });
    }
    (out, refused)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, requires: &[&str], after: &[&str]) -> Candidate {
        Candidate {
            name: name.into(),
            requires: requires.iter().map(|s| s.to_string()).collect(),
            after: after.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn names(cands: &[Candidate], idx: &[usize]) -> Vec<String> {
        idx.iter().map(|&i| cands[i].name.clone()).collect()
    }

    #[test]
    fn nothing_declared_is_the_order_given() {
        let cands = [c("sidebar", &[], &[]), c("git", &[], &[]), c("usage", &[], &[])];
        let (o, r) = order(&cands);
        assert_eq!(names(&cands, &o), ["sidebar", "git", "usage"]);
        assert!(r.is_empty());
    }

    #[test]
    fn a_requirement_moves_ahead_and_nothing_else_moves() {
        let cands = [c("badges", &["sidebar"], &[]), c("git", &[], &[]), c("sidebar", &[], &[])];
        let (o, r) = order(&cands);
        assert_eq!(names(&cands, &o), ["git", "sidebar", "badges"]);
        assert!(r.is_empty());
    }

    #[test]
    fn a_missing_requirement_drops_the_plugin_and_its_dependents() {
        let cands = [
            c("a", &["nope"], &[]),
            c("b", &["a"], &[]),
            c("c", &[], &[]),
        ];
        let (o, r) = order(&cands);
        assert_eq!(names(&cands, &o), ["c"]);
        assert_eq!(r, [
            Refused::MissingRequirement { plugin: "a".into(), requires: "nope".into() },
            Refused::MissingRequirement { plugin: "b".into(), requires: "a".into() },
        ]);
    }

    #[test]
    fn after_is_satisfied_by_absence() {
        let cands = [c("mine", &[], &["usage"]), c("sidebar", &[], &[])];
        let (o, r) = order(&cands);
        assert_eq!(names(&cands, &o), ["mine", "sidebar"]);
        assert!(r.is_empty());
    }

    #[test]
    fn after_orders_when_both_are_present() {
        let cands = [c("mine", &[], &["usage"]), c("usage", &[], &[])];
        let (o, _) = order(&cands);
        assert_eq!(names(&cands, &o), ["usage", "mine"]);
    }

    #[test]
    fn a_cycle_refuses_both_and_loads_the_rest() {
        let cands = [c("a", &["b"], &[]), c("b", &["a"], &[]), c("c", &[], &[])];
        let (o, r) = order(&cands);
        assert_eq!(names(&cands, &o), ["c"]);
        assert_eq!(r, [Refused::Cycle { plugin: "a".into() }, Refused::Cycle { plugin: "b".into() }]);
    }

    #[test]
    fn requiring_yourself_is_ignored() {
        let cands = [c("a", &["a"], &[])];
        let (o, r) = order(&cands);
        assert_eq!(names(&cands, &o), ["a"]);
        assert!(r.is_empty());
    }
}
