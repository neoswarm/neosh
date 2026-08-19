//! Settings, as data.
//!
//! An option is a *declared* name with a type, a default and an owner. Nothing is settable that
//! was not declared first, which is what makes a typo in a config file an error at the line that
//! caused it rather than a setting that silently does nothing.
//!
//! The built-in options are declared through [`ApiCall::OptDeclare`](crate::api::ApiCall::OptDeclare)
//! by the host at startup, using the same call a plugin uses. There is no separate built-in table.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// What an option accepts.
///
/// The constraint travels with the declaration rather than living in the setter, so a settings UI
/// can render a correct control for an option it has never heard of.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
#[ts(export)]
pub enum OptionType {
    Bool,
    Int {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "number | null")]
        min: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "number | null")]
        max: Option<i64>,
    },
    Float,
    Str,
    /// A closed set of strings. Anything else is rejected at set time, with the valid values in the
    /// error message.
    Enum {
        values: Vec<String>,
    },
    /// A list of strings.
    List,
    /// Anything JSON. The escape hatch for plugins with structured settings; unvalidated by the
    /// host, so the plugin owns its own schema.
    Json,
}

impl OptionType {
    pub fn describe(&self) -> String {
        match self {
            Self::Bool => "boolean".into(),
            Self::Int { min: None, max: None } => "integer".into(),
            Self::Int { min, max } => match (min, max) {
                (Some(lo), Some(hi)) => format!("integer in {lo}..={hi}"),
                (Some(lo), None) => format!("integer >= {lo}"),
                (None, Some(hi)) => format!("integer <= {hi}"),
                (None, None) => "integer".into(),
            },
            Self::Float => "number".into(),
            Self::Str => "string".into(),
            Self::Enum { values } => format!("one of {}", values.join(", ")),
            Self::List => "list of strings".into(),
            Self::Json => "json".into(),
        }
    }
}

/// An option's value.
///
/// Untagged on purpose: a config file says `border = "rounded"`, not
/// `border = { kind = "str", value = "rounded" }`. Deserialization tries the variants in
/// declaration order, so `true` is a `Bool` and `3` is an `Int` rather than both landing in
/// `Json`. Type checking happens against the declaration, which is where the type actually is.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(untagged)]
#[ts(export)]
pub enum OptionValue {
    Bool(bool),
    #[ts(type = "number")]
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<String>),
    /// Structured settings a plugin validates itself.
    ///
    /// Typed as an object rather than `unknown`: a bare `unknown` in a union swallows every other
    /// member, and `opt.set(name, anything)` type-checking is exactly the drift this project is
    /// trying to avoid.
    Json(#[ts(type = "{ [key: string]: unknown }")] serde_json::Value),
}

impl OptionValue {
    /// A short rendering for error messages and `:set?`-style listings.
    pub fn display(&self) -> String {
        match self {
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::Float(f) => f.to_string(),
            Self::Str(s) => s.clone(),
            Self::List(v) => v.join(","),
            Self::Json(v) => v.to_string(),
        }
    }

    /// The string behind a `Str` or `Enum` option, if that is what this is.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s.as_str()),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(i) => Some(*i),
            _ => None,
        }
    }
}

/// What a declaration says.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct OptionSpec {
    /// Dotted, lowercase: `agent.model`, `myplugin.enabled`. Validated at declaration so the name
    /// space stays greppable and a future `:set` grammar has something regular to parse.
    pub name: String,
    #[serde(rename = "type")]
    pub ty: OptionType,
    pub default: OptionValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A declared option with its current value, as listed.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct OptionEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: OptionType,
    pub value: OptionValue,
    pub default: OptionValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Who declared it. `neosh` for built-ins; otherwise the plugin id.
    pub owner: String,
    /// False while the option still holds its default, so a settings UI can show what the user
    /// actually changed and a `:set` persister knows what is worth writing down.
    pub modified: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_as_bare_json() {
        // The whole point of untagged: a config file and a plugin write the natural literal.
        let cases = [
            ("true", OptionValue::Bool(true)),
            ("42", OptionValue::Int(42)),
            ("1.5", OptionValue::Float(1.5)),
            ("\"rounded\"", OptionValue::Str("rounded".into())),
            ("[\"a\",\"b\"]", OptionValue::List(vec!["a".into(), "b".into()])),
        ];
        for (json, want) in cases {
            let got: OptionValue = serde_json::from_str(json).unwrap();
            assert_eq!(got, want, "parsing {json}");
            assert_eq!(serde_json::to_string(&want).unwrap(), json);
        }
    }

    #[test]
    fn an_integer_does_not_become_a_float() {
        // Variant order decides this, and getting it wrong would turn every line count into 3.0.
        assert_eq!(serde_json::from_str::<OptionValue>("3").unwrap(), OptionValue::Int(3));
    }

    #[test]
    fn a_mixed_array_falls_through_to_json_rather_than_failing() {
        let got: OptionValue = serde_json::from_str("[1,\"a\"]").unwrap();
        assert!(matches!(got, OptionValue::Json(_)));
    }

    #[test]
    fn constraints_describe_themselves_for_error_messages() {
        assert_eq!(
            OptionType::Int { min: Some(1), max: Some(9) }.describe(),
            "integer in 1..=9"
        );
        assert_eq!(
            OptionType::Enum { values: vec!["a".into(), "b".into()] }.describe(),
            "one of a, b"
        );
    }
}
