//! The option registry.
//!
//! Declare-before-set, with the type checked against the declaration. Two consequences follow, and
//! both are the point:
//!
//! * A misspelled option in a config file fails at the line that set it, naming the option and
//!   listing what was valid. A config format where a typo is indistinguishable from a setting that
//!   does not work is a support burden that never ends.
//! * A settings UI can be written once, against [`OptionEntry`], and render options belonging to
//!   plugins it has never heard of.
//!
//! Values are stored, not interpreted. Whoever declared an option is responsible for acting on it;
//! the registry's job is to say what exists, what type it is, and what it currently holds.

use std::collections::BTreeMap;

use neosh_proto::{ApiError, OptionEntry, OptionSpec, OptionType, OptionValue};

#[derive(Debug, Clone)]
struct Entry {
    spec: OptionSpec,
    owner: String,
    value: Option<OptionValue>,
}

/// Declared options, keyed by name.
///
/// `BTreeMap` rather than `HashMap`: listing options is a user-facing operation and alphabetical
/// beats whatever the hasher decided.
#[derive(Debug, Default, Clone)]
pub struct OptionRegistry {
    entries: BTreeMap<String, Entry>,
}

impl OptionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare an option.
    ///
    /// Re-declaring an option you already own replaces the spec and keeps any explicitly set
    /// value, so a plugin reloading does not reset the user's settings. Declaring a name someone
    /// else owns is refused: silent shadowing would make "why did my setting stop working" a
    /// question with no findable answer.
    pub fn declare(&mut self, owner: &str, spec: OptionSpec) -> Result<(), ApiError> {
        validate_name(&spec.name)?;
        check(&spec.ty, &spec.default).map_err(|why| ApiError::InvalidArgument {
            message: format!("option {}: default {why}", spec.name),
        })?;

        match self.entries.get_mut(&spec.name) {
            Some(existing) if existing.owner != owner => Err(ApiError::InvalidArgument {
                message: format!("option {} is already declared by {}", spec.name, existing.owner),
            }),
            Some(existing) => {
                // Keep an explicitly set value only if it still fits the new declaration.
                if let Some(v) = &existing.value
                    && check(&spec.ty, v).is_err()
                {
                    existing.value = None;
                }
                existing.spec = spec;
                Ok(())
            }
            None => {
                self.entries.insert(
                    spec.name.clone(),
                    Entry { spec, owner: owner.to_string(), value: None },
                );
                Ok(())
            }
        }
    }

    /// Set an option, returning the normalized value actually stored.
    ///
    /// Normalization is narrow on purpose — an integer where a float was declared, because JSON
    /// has one number type and `1` is a perfectly good `1.0`. Everything else is a type error,
    /// because a config language that coerces `"true"` into `true` teaches people to write
    /// nonsense that happens to work.
    pub fn set(&mut self, name: &str, value: OptionValue) -> Result<OptionValue, ApiError> {
        if !self.entries.contains_key(name) {
            return Err(unknown(name, &self.entries));
        }
        let entry = self.entries.get_mut(name).expect("just checked");
        let value = normalize(&entry.spec.ty, value).map_err(|why| ApiError::InvalidArgument {
            message: format!("option {name}: {why}"),
        })?;
        entry.value = Some(value.clone());
        Ok(value)
    }

    /// Restore an option to its default, returning that default.
    pub fn reset(&mut self, name: &str) -> Result<OptionValue, ApiError> {
        if !self.entries.contains_key(name) {
            return Err(unknown(name, &self.entries));
        }
        let entry = self.entries.get_mut(name).expect("just checked");
        entry.value = None;
        Ok(entry.spec.default.clone())
    }

    /// The current value, or `None` if the option was never declared.
    pub fn get(&self, name: &str) -> Option<&OptionValue> {
        let e = self.entries.get(name)?;
        Some(e.value.as_ref().unwrap_or(&e.spec.default))
    }

    pub fn entry(&self, name: &str) -> Option<OptionEntry> {
        self.entries.get(name).map(|e| render(name, e))
    }

    pub fn all(&self) -> Vec<OptionEntry> {
        self.entries.iter().map(|(name, e)| render(name, e)).collect()
    }

    /// Drop everything an unloading plugin declared. Its options stop existing rather than
    /// lingering as settable names nothing reads.
    pub fn remove_owner(&mut self, owner: &str) {
        self.entries.retain(|_, e| e.owner != owner);
    }

    // ---- typed reads, for the host --------------------------------------

    pub fn str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(OptionValue::as_str).filter(|s| !s.is_empty())
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        self.get(name).and_then(OptionValue::as_bool)
    }

    pub fn int(&self, name: &str) -> Option<i64> {
        self.get(name).and_then(OptionValue::as_int)
    }

    /// A list option's entries. `None` when it is unset *or* empty, which for a list of things to
    /// do is the same answer: nothing to do.
    pub fn list(&self, name: &str) -> Option<Vec<String>> {
        self.get(name)
            .and_then(OptionValue::as_list)
            .filter(|v| !v.is_empty())
            .map(|v| v.to_vec())
    }
}

fn render(name: &str, e: &Entry) -> OptionEntry {
    OptionEntry {
        name: name.to_string(),
        ty: e.spec.ty.clone(),
        value: e.value.clone().unwrap_or_else(|| e.spec.default.clone()),
        default: e.spec.default.clone(),
        description: e.spec.description.clone(),
        owner: e.owner.clone(),
        modified: e.value.is_some(),
    }
}

/// "No such option" plus the closest declared name, because the overwhelmingly likely cause is a
/// typo and the fix is one character away.
fn unknown(name: &str, entries: &BTreeMap<String, Entry>) -> ApiError {
    let suggestion = entries
        .keys()
        .map(|k| (distance(name, k), k))
        .filter(|(d, k)| *d <= 3 && *d < k.len())
        .min()
        .map(|(_, k)| format!(", did you mean {k}?"))
        .unwrap_or_default();
    ApiError::NotFound { what: format!("option {name} is not declared{suggestion}") }
}

fn validate_name(name: &str) -> Result<(), ApiError> {
    let ok = !name.is_empty()
        && name.split('.').all(|seg| {
            !seg.is_empty()
                && seg.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        });
    if ok {
        Ok(())
    } else {
        Err(ApiError::InvalidArgument {
            message: format!(
                "option name {name:?} must be dot-separated lowercase segments, e.g. \
                 \"myplugin.enabled\""
            ),
        })
    }
}

fn check(ty: &OptionType, value: &OptionValue) -> Result<(), String> {
    normalize(ty, value.clone()).map(|_| ())
}

fn normalize(ty: &OptionType, value: OptionValue) -> Result<OptionValue, String> {
    use OptionValue as V;
    match (ty, value) {
        (OptionType::Bool, v @ V::Bool(_)) => Ok(v),
        (OptionType::Int { min, max }, V::Int(i)) => {
            if min.is_some_and(|lo| i < lo) || max.is_some_and(|hi| i > hi) {
                return Err(format!("{i} is out of range, expected {}", ty.describe()));
            }
            Ok(V::Int(i))
        }
        // JSON has one number type; an integer literal for a float option is not a mistake.
        (OptionType::Float, V::Int(i)) => Ok(V::Float(i as f64)),
        (OptionType::Float, v @ V::Float(_)) => Ok(v),
        (OptionType::Str, v @ V::Str(_)) => Ok(v),
        (OptionType::Enum { values }, V::Str(s)) => {
            if values.contains(&s) {
                Ok(V::Str(s))
            } else {
                Err(format!("{s:?} is not valid, expected {}", ty.describe()))
            }
        }
        (OptionType::List, v @ V::List(_)) => Ok(v),
        // An empty JSON array is ambiguous between a list and structured data.
        (OptionType::List, V::Json(serde_json::Value::Array(a))) if a.is_empty() => {
            Ok(V::List(Vec::new()))
        }
        (OptionType::Json, v) => Ok(V::Json(
            serde_json::to_value(&v).unwrap_or(serde_json::Value::Null),
        )),
        (_, v) => Err(format!("expected {}, got {}", ty.describe(), kind_of(&v))),
    }
}

fn kind_of(v: &OptionValue) -> &'static str {
    match v {
        OptionValue::Bool(_) => "boolean",
        OptionValue::Int(_) | OptionValue::Float(_) => "number",
        OptionValue::Str(_) => "string",
        OptionValue::List(_) => "list",
        OptionValue::Json(_) => "json",
    }
}

/// Levenshtein, bounded by the short names options actually have.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, ty: OptionType, default: OptionValue) -> OptionSpec {
        OptionSpec { name: name.into(), ty, default, description: None }
    }

    fn reg() -> OptionRegistry {
        let mut r = OptionRegistry::new();
        r.declare("neosh", spec("agent.model", OptionType::Str, OptionValue::Str(String::new())))
            .unwrap();
        r.declare(
            "neosh",
            spec("chat.scrollback", OptionType::Int { min: Some(0), max: Some(100_000) }, OptionValue::Int(10_000)),
        )
        .unwrap();
        r.declare(
            "neosh",
            spec(
                "ui.border",
                OptionType::Enum { values: vec!["none".into(), "rounded".into()] },
                OptionValue::Str("rounded".into()),
            ),
        )
        .unwrap();
        r
    }

    #[test]
    fn an_undeclared_option_cannot_be_set() {
        // The entire reason for declare-before-set: a typo must not look like a working setting.
        let mut r = reg();
        assert!(r.set("agent.modle", OptionValue::Str("x".into())).is_err());
    }

    #[test]
    fn a_near_miss_suggests_the_real_name() {
        let mut r = reg();
        let err = r.set("agent.modle", OptionValue::Str("x".into())).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("agent.model"), "{msg}");
    }

    #[test]
    fn a_wrong_type_is_refused_rather_than_coerced() {
        let mut r = reg();
        assert!(r.set("chat.scrollback", OptionValue::Str("lots".into())).is_err());
        assert!(r.set("agent.model", OptionValue::Bool(true)).is_err());
    }

    #[test]
    fn an_integer_is_accepted_where_a_float_was_declared() {
        // JSON has one number type. Refusing `1` for a float would be pedantry with no upside.
        let mut r = OptionRegistry::new();
        r.declare("p", spec("p.ratio", OptionType::Float, OptionValue::Float(0.5))).unwrap();
        assert_eq!(r.set("p.ratio", OptionValue::Int(1)).unwrap(), OptionValue::Float(1.0));
    }

    #[test]
    fn an_out_of_range_integer_is_refused_and_says_the_range() {
        let mut r = reg();
        let err = r.set("chat.scrollback", OptionValue::Int(-1)).unwrap_err();
        assert!(format!("{err:?}").contains("0..=100000"));
    }

    #[test]
    fn an_enum_only_accepts_its_declared_values() {
        let mut r = reg();
        assert!(r.set("ui.border", OptionValue::Str("rounded".into())).is_ok());
        let err = r.set("ui.border", OptionValue::Str("fancy".into())).unwrap_err();
        assert!(format!("{err:?}").contains("none, rounded"));
    }

    #[test]
    fn get_falls_back_to_the_default_until_something_sets_it() {
        let mut r = reg();
        assert_eq!(r.get("chat.scrollback"), Some(&OptionValue::Int(10_000)));
        assert!(!r.entry("chat.scrollback").unwrap().modified);
        r.set("chat.scrollback", OptionValue::Int(5)).unwrap();
        assert_eq!(r.get("chat.scrollback"), Some(&OptionValue::Int(5)));
        assert!(r.entry("chat.scrollback").unwrap().modified, "a settings UI needs to know");
    }

    #[test]
    fn reset_restores_the_default() {
        let mut r = reg();
        r.set("chat.scrollback", OptionValue::Int(5)).unwrap();
        assert_eq!(r.reset("chat.scrollback").unwrap(), OptionValue::Int(10_000));
        assert!(!r.entry("chat.scrollback").unwrap().modified);
    }

    #[test]
    fn one_plugin_cannot_declare_over_another() {
        let mut r = reg();
        let err = r
            .declare("evil", spec("agent.model", OptionType::Bool, OptionValue::Bool(false)))
            .unwrap_err();
        assert!(format!("{err:?}").contains("neosh"), "the error names the real owner");
    }

    #[test]
    fn redeclaring_your_own_option_keeps_the_users_value() {
        // Plugin reload must not silently reset settings back to defaults.
        let mut r = reg();
        r.set("agent.model", OptionValue::Str("anthropic/claude-opus-5".into())).unwrap();
        r.declare("neosh", spec("agent.model", OptionType::Str, OptionValue::Str(String::new())))
            .unwrap();
        assert_eq!(r.str("agent.model"), Some("anthropic/claude-opus-5"));
    }

    #[test]
    fn redeclaring_with_an_incompatible_type_drops_the_stale_value() {
        let mut r = OptionRegistry::new();
        r.declare("p", spec("p.x", OptionType::Str, OptionValue::Str("a".into()))).unwrap();
        r.set("p.x", OptionValue::Str("b".into())).unwrap();
        r.declare("p", spec("p.x", OptionType::Bool, OptionValue::Bool(false))).unwrap();
        assert_eq!(r.get("p.x"), Some(&OptionValue::Bool(false)), "must not keep a string");
    }

    #[test]
    fn a_declaration_whose_default_does_not_fit_its_type_is_refused() {
        let mut r = OptionRegistry::new();
        assert!(
            r.declare("p", spec("p.x", OptionType::Bool, OptionValue::Int(1))).is_err(),
            "otherwise get() returns a value set() would reject"
        );
    }

    #[test]
    fn names_must_be_dotted_lowercase() {
        let mut r = OptionRegistry::new();
        for bad in ["Agent.Model", "agent..model", "", "agent.model!", "agent-model"] {
            assert!(
                r.declare("p", spec(bad, OptionType::Bool, OptionValue::Bool(false))).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert!(r.declare("p", spec("a1.b_2.c", OptionType::Bool, OptionValue::Bool(false))).is_ok());
    }

    #[test]
    fn unloading_a_plugin_takes_its_options_with_it() {
        let mut r = reg();
        r.declare("mine", spec("mine.on", OptionType::Bool, OptionValue::Bool(true))).unwrap();
        r.remove_owner("mine");
        assert!(r.get("mine.on").is_none());
        assert!(r.get("agent.model").is_some(), "other owners are untouched");
    }

    #[test]
    fn listing_is_alphabetical_so_a_settings_screen_is_stable() {
        let r = reg();
        let names: Vec<_> = r.all().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["agent.model", "chat.scrollback", "ui.border"]);
    }

    #[test]
    fn an_empty_string_option_reads_as_unset() {
        // How "no preference, use the built-in behaviour" is spelled for strings.
        let r = reg();
        assert_eq!(r.str("agent.model"), None);
    }
}
