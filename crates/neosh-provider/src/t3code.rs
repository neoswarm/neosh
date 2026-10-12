//! t3code's model manifest, read as news about our catalogue.
//!
//! [t3code](https://github.com/pingdotgg/t3code) drives the same vendor CLIs this workspace does and
//! keeps a model manifest on its `main` (MIT) that it updates the day a model ships: which models
//! the `claude`, `codex` and `agy` CLIs serve, which of them are current, and which CLI release
//! each one needs. Following it means a model released this morning is in `^P` this afternoon
//! without anybody here editing anything.
//!
//! # News, not a replacement
//!
//! Its vocabulary is not ours — its knobs are translated by its adapters, ours are read by our
//! drivers — and it says nothing about the API catalogues, prices or taglines. So it is laid
//! *over* `models.json` ([`apply`]) rather than in place of it, and what it may change is narrow:
//!
//! * **What exists.** A model it lists that we do not is added, in its place in their order but
//!   never above our first row, which is what a fresh install defaults to.
//! * **What is current.** `legacy` follows its `status`, because "superseded" is a fact that
//!   changes the day the next model ships, and is exactly what a release-bound list gets wrong.
//! * **Which CLI it needs**, where we have not written one down. Where we have, ours stands: the
//!   floors in `models.json` were read off the CLI's own refusals.
//!
//! A model it adds borrows its family, rung, tagline and knobs from a sibling we already describe
//! — one it files under the same profile, or one in the line its alias names (`haiku`) — because
//! those are editorial and a guess from an id is the one thing they must not be. One with no
//! sibling gets its knobs translated from its profile and is left off the ladder rather than put
//! on a rung nobody chose.
//!
//! Read defensively throughout: it is somebody else's file and owes us no stability. A field we do
//! not understand is skipped, and a file with nothing usable in it is refused whole.

use std::collections::BTreeMap;

use neosh_proto::{OptionChoice, ProviderOptionDescriptor};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manifest::{Entry, Manifest, parse_version, timestamp_secs};

/// Where t3code keeps it.
pub const URL: &str =
    "https://raw.githubusercontent.com/pingdotgg/t3code/main/apps/server/src/provider/model-manifest.json";

/// Their driver, and the catalogues of ours it describes. `claudeAgent` is the `claude` CLI, and
/// its models are also what `api.anthropic.com` serves — so a new Claude is news for both.
const TARGETS: &[(&str, &[&str])] = &[
    ("claudeAgent", &["claude-cli", "anthropic"]),
    ("codex", &["codex-cli"]),
    ("antigravity", &["antigravity"]),
];

/// The words a harness reads out of a message, in the order our ladders put them.
const WORDS: &[&str] = &["ultracode", "ultrathink"];

/// What we kept of their file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Overlay {
    /// Their `updatedAt`, as `YYYY-MM-DDTHH:MM:SSZ`. Comparable between two of *theirs* and with
    /// nothing of ours.
    pub updated_at: String,
    /// Keyed by *their* driver name.
    pub providers: BTreeMap<String, Provider>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    pub models: Vec<Model>,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub slug: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub current: bool,
    pub profile: Option<String>,
    pub min_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Their option descriptors, untranslated.
    pub options: Vec<Value>,
    /// The context window, when the profile fixes one or offers a choice of them.
    pub context: Option<u64>,
}

/// Read their file. Refused when nothing in it is usable.
pub fn parse(text: &str) -> Result<Overlay, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let stamp = v.get("updatedAt").and_then(Value::as_str).ok_or("no updatedAt")?;
    let updated_at = normalise_stamp(stamp).ok_or_else(|| format!("updatedAt {stamp:?} is not a date"))?;
    let providers_v = v.get("providers").and_then(Value::as_object).ok_or("no providers")?;
    let current_lists = v.get("currentModels").and_then(Value::as_object);

    let mut providers = BTreeMap::new();
    for (driver, _) in TARGETS {
        let Some(p) = providers_v.get(*driver) else { continue };
        let listed_current: Vec<&str> = current_lists
            .and_then(|c| c.get(*driver))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut models = Vec::new();
        for m in p.get("models").and_then(Value::as_array).into_iter().flatten() {
            let (Some(slug), Some(name)) = (
                m.get("slug").and_then(Value::as_str).filter(|s| !s.trim().is_empty()),
                m.get("name").and_then(Value::as_str).filter(|s| !s.trim().is_empty()),
            ) else {
                continue;
            };
            if models.iter().any(|x: &Model| x.slug == slug) {
                continue;
            }
            let current = match m.get("status").and_then(Value::as_str) {
                Some(status) => status == "current",
                None => listed_current.contains(&slug),
            };
            // Whichever adapter says it: `claudeCode`, `codex`, and whatever they add next.
            let min_version = m
                .get("adapter")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|a| a.values())
                .filter_map(|a| a.get("minVersion").and_then(Value::as_str))
                .find(|v| parse_version(v).is_some())
                .map(str::to_string);
            models.push(Model {
                slug: slug.to_string(),
                name: name.to_string(),
                aliases: m
                    .get("aliases")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default(),
                current,
                profile: m.get("profile").and_then(Value::as_str).map(str::to_string),
                min_version,
            });
        }
        let mut profiles = BTreeMap::new();
        for (name, prof) in p.get("profiles").and_then(Value::as_object).into_iter().flatten() {
            let options = prof
                .pointer("/capabilities/optionDescriptors")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let adapter = prof.pointer("/adapter/claudeCode");
            let context = adapter
                .and_then(|a| a.get("fixedContextWindowTokens"))
                .and_then(Value::as_u64)
                .or_else(|| {
                    adapter
                        .and_then(|a| a.get("contextWindowTokens"))
                        .and_then(Value::as_object)
                        .and_then(|t| t.values().filter_map(Value::as_u64).max())
                });
            profiles.insert(name.clone(), Profile { options, context });
        }
        if !models.is_empty() {
            providers.insert(driver.to_string(), Provider { models, profiles });
        }
    }
    if providers.is_empty() {
        return Err("describes none of the CLIs this workspace drives".into());
    }
    Ok(Overlay { updated_at, providers })
}

/// `2026-10-09T23:31:10Z`, `2026-10-09T23:31:10.123Z` -> the first. Fractions are dropped so two
/// of their stamps compare as strings.
fn normalise_stamp(s: &str) -> Option<String> {
    let s = s.trim();
    let whole = match s.find('.') {
        Some(dot) if s.ends_with('Z') => format!("{}Z", &s[..dot]),
        _ => s.to_string(),
    };
    timestamp_secs(&whole).map(|_| whole)
}

/// Our catalogue, with their news laid over it.
pub fn apply(base: &Manifest, overlay: &Overlay) -> Manifest {
    let mut out = base.clone();
    for (driver, targets) in TARGETS {
        let Some(theirs) = overlay.providers.get(*driver) else { continue };
        for target in *targets {
            lay_over(&mut out, target, theirs);
        }
    }
    out
}

fn lay_over(out: &mut Manifest, target: &str, theirs: &Provider) {
    // The API serves the model and reads no words, takes no `[1m]` suffix and has no CLI to be too
    // old: what is about the harness stays on the harness's catalogue.
    let api = target == "anthropic";
    let Some(cat) = out.catalogues.get(target) else { return };
    let mut models = cat.models.clone();

    // Ours, as they file them: which of theirs is which of ours, for finding a sibling by profile.
    let profile_of = |slug: &str| theirs.models.iter().find(|m| m.slug == slug).and_then(|m| m.profile.clone());

    for m in &mut models {
        if let Some(t) = theirs.models.iter().find(|t| t.slug == m.id) {
            m.legacy = !t.current;
            if !api && m.min_version.is_none() {
                m.min_version = t.min_version.clone();
            }
        }
    }

    // Only what is current is news. A model they list as superseded that we never listed was
    // retired before anybody here could have picked it, and a row for it is clutter in `^P`.
    //
    // Each goes directly above the model it succeeds — the newest of ours in its line — and
    // otherwise where their order puts it; never above our first row while that row is current,
    // because the first row is what a fresh install lands on and somebody else's ordering should
    // not move it. Once they supersede it, it is legacy and the successor may take its place.
    let mut last: Option<(String, usize)> = None;
    for (i, t) in theirs.models.iter().enumerate() {
        if !t.current || models.iter().any(|m| m.id == t.slug) {
            continue;
        }
        let (entry, sibling) = describe_new(out, target, api, theirs, t, &models, &profile_of);
        let anchor = sibling.or_else(|| {
            theirs.models[i + 1..]
                .iter()
                .find(|n| models.iter().any(|m| m.id == n.slug))
                .map(|n| n.slug.clone())
        });
        let mut at = anchor
            .as_ref()
            .and_then(|a| models.iter().position(|m| &m.id == a))
            .unwrap_or(models.len());
        if models.first().is_some_and(|m| !m.legacy) {
            at = at.max(1);
        }
        // Two placed against one anchor keep their order between them.
        if let (Some(a), Some((prev, idx))) = (&anchor, &last)
            && a == prev
        {
            at = at.max(idx + 1);
        }
        models.insert(at, entry);
        last = anchor.map(|a| (a, at));
    }

    if let Some(cat) = out.catalogues.get_mut(target) {
        cat.models = models;
    }
}

fn describe_new(
    out: &mut Manifest,
    target: &str,
    api: bool,
    theirs: &Provider,
    t: &Model,
    known: &[Entry],
    profile_of: &dyn Fn(&str) -> Option<String>,
) -> (Entry, Option<String>) {
    let profile = t.profile.as_ref().and_then(|p| theirs.profiles.get(p));
    // A sibling: one of ours they file under the same profile — the same model a release on — and
    // failing that, the line their alias names. `haiku` is the CLI's own name for the line.
    let by_profile = t
        .profile
        .as_ref()
        .and_then(|p| known.iter().find(|k| profile_of(&k.id).as_ref() == Some(p)));
    let by_line = || {
        t.aliases
            .iter()
            .filter(|a| !a.chars().any(|c| c.is_ascii_digit()))
            .find_map(|a| known.iter().find(|k| k.family.as_deref() == Some(a.as_str())))
    };
    let sibling = by_profile.or_else(by_line);

    let options = match (by_profile, profile) {
        // Same profile, same knobs: and ours are the ones our drivers read.
        (Some(s), _) => s.options.clone(),
        (None, Some(p)) => translate(out, target, api, t, p),
        (None, None) => out
            .catalogues
            .get(target)
            .and_then(|c| c.rules.iter().find(|r| t.slug.starts_with(r.prefix.as_str())))
            .map(|r| r.options.clone())
            .unwrap_or_default(),
    };

    // The same model under another of our catalogues — `gpt-6-astra` is on `openai` before it
    // is on `codex-cli` — says what it is better than a sibling does.
    let same = out
        .catalogues
        .iter()
        .filter(|(name, _)| name.as_str() != target)
        .find_map(|(_, c)| c.models.iter().find(|e| e.id == t.slug))
        .cloned();
    let about = same.as_ref().or(sibling);
    let entry = Entry {
        id: t.slug.clone(),
        name: t.name.clone(),
        family: about.and_then(|s| s.family.clone()),
        tier: about.and_then(|s| s.tier),
        tagline: about.and_then(|s| s.tagline.clone()),
        legacy: !t.current,
        context: profile.and_then(|p| p.context).or_else(|| about.and_then(|s| s.context)),
        max_output: about.and_then(|s| s.max_output),
        // A price is not something to borrow: a new model in an old line is very often a new
        // price, and a wrong number on `^L` is money reported wrongly.
        pricing: None,
        options,
        // Whether it reasons goes with the knobs: borrowed with them, and otherwise read off them.
        thinking: by_profile.and_then(|s| s.thinking),
        min_version: if api { None } else { t.min_version.clone() },
    };
    (entry, sibling.map(|s| s.id.clone()))
}

/// Their option descriptors, as knobs our drivers read. Only the knobs we know how to send survive:
/// one we cannot drive would be a control that silently does nothing.
fn translate(out: &mut Manifest, target: &str, api: bool, t: &Model, p: &Profile) -> Vec<String> {
    let mut names = Vec::new();
    for d in &p.options {
        let id = d.get("id").and_then(Value::as_str).unwrap_or_default();
        match id {
            "effort" => {
                let name = format!("t3code/{target}/{}/effort", t.slug);
                if let Some(knob) = effort(out, api, d) {
                    out.knobs.insert(name.clone(), knob);
                    names.push(name);
                }
            }
            "fastMode" if out.knobs.contains_key("fast-mode") => names.push("fast-mode".into()),
            "thinking" if out.knobs.contains_key("thinking") => names.push("thinking".into()),
            "contextWindow" if !api => {
                let long = d
                    .get("options")
                    .and_then(Value::as_array)
                    .and_then(|o| o.iter().find(|c| c.get("isDefault").and_then(Value::as_bool) == Some(true)))
                    .and_then(|c| c.get("id").and_then(Value::as_str))
                    == Some("1m");
                let knob = if long { "context-1m-default" } else { "context-200k-default" };
                if out.knobs.contains_key(knob) {
                    names.push(knob.into());
                }
            }
            _ => {}
        }
    }
    names
}

fn effort(out: &Manifest, api: bool, d: &Value) -> Option<ProviderOptionDescriptor> {
    let choices = d.get("options").and_then(Value::as_array)?;
    let ids: Vec<&str> = choices.iter().filter_map(|c| c.get("id").and_then(Value::as_str)).collect();
    let default = choices
        .iter()
        .find(|c| c.get("isDefault").and_then(Value::as_bool) == Some(true))
        .and_then(|c| c.get("id").and_then(Value::as_str))
        .filter(|id| !WORDS.contains(id))
        .unwrap_or("high");
    let levels: Vec<&str> = ids.iter().copied().filter(|id| !WORDS.contains(id)).collect();
    if levels.is_empty() {
        return None;
    }
    // The words read like ours wherever they came from: the label and the sentence under it are
    // borrowed from whichever ladder of ours already offers the word.
    let words: Vec<&str> = if api { Vec::new() } else { WORDS.iter().copied().filter(|w| ids.contains(w)).collect() };
    let described = |w: &str| {
        out.knobs.values().find_map(|k| match k {
            ProviderOptionDescriptor::Select { options, .. } => options.iter().find(|o| o.id == w).cloned(),
            _ => None,
        })
    };
    let mut options: Vec<OptionChoice> = levels
        .iter()
        .map(|l| OptionChoice {
            id: (*l).into(),
            label: crate::catalog::level_label(l),
            description: None,
            is_default: *l == default,
        })
        .collect();
    options.extend(words.iter().map(|w| {
        described(w).unwrap_or_else(|| OptionChoice {
            id: (*w).into(),
            label: crate::catalog::title_case(w),
            description: None,
            is_default: false,
        })
    }));
    Some(ProviderOptionDescriptor::Select {
        id: "effort".into(),
        label: "Effort".into(),
        description: Some("How much reasoning the model spends before answering.".into()),
        options,
        current_value: Some(default.into()),
        prompt_injected_values: words.iter().map(|w| w.to_string()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic models only: the real ones are data, and a test that repeats data is a second
    /// place to update every time a vendor ships.
    fn base() -> Manifest {
        crate::manifest::parse(
            &serde_json::json!({
                "version": 1,
                "knobs": {
                    "fast-mode": { "type": "boolean", "id": "fast_mode", "label": "Fast mode" },
                    "ladder": { "type": "select", "id": "effort", "label": "Effort", "options": [
                        { "id": "low", "label": "Low" }, { "id": "high", "label": "High", "is_default": true },
                        { "id": "ultrathink", "label": "Ultrathink", "description": "Past the top." }
                    ], "prompt_injected_values": ["ultrathink"] }
                },
                "catalogues": {
                    "claude-cli": { "models": [
                        { "id": "big-2", "name": "Big 2", "family": "big", "tier": "frontier", "tagline": "Most", "options": ["ladder"], "min_version": "1.2.0" },
                        { "id": "small-1", "name": "Small 1", "family": "small", "tier": "fast", "tagline": "Least" }
                    ] },
                    "anthropic": { "models": [
                        { "id": "big-2", "name": "Big 2", "family": "big", "tier": "frontier", "tagline": "Most" }
                    ] }
                }
            })
            .to_string(),
        )
        .expect("a valid base")
    }

    fn theirs(models: Value) -> Overlay {
        parse(
            &serde_json::json!({
                "updatedAt": "2026-10-09T23:31:10.500Z",
                "providers": { "claudeAgent": {
                    "profiles": {
                        "big": { "capabilities": { "optionDescriptors": [] } },
                        "tiny": { "capabilities": { "optionDescriptors": [
                            { "id": "effort", "type": "select", "options": [
                                { "id": "low", "label": "Low", "isDefault": true },
                                { "id": "max", "label": "Max" }, { "id": "ultrathink", "label": "U" } ] },
                            { "id": "fastMode", "type": "boolean" },
                            { "id": "somethingNew", "type": "boolean" }
                        ] }, "adapter": { "claudeCode": { "fixedContextWindowTokens": 500000 } } }
                    },
                    "models": models
                } }
            })
            .to_string(),
        )
        .expect("their shape")
    }

    #[test]
    fn a_model_they_list_and_we_do_not_arrives_described_by_its_sibling() {
        let o = theirs(serde_json::json!([
            { "slug": "big-3", "name": "Big 3", "status": "current", "profile": "big",
              "adapter": { "claudeCode": { "minVersion": "1.4.0" } } },
            { "slug": "big-2", "name": "Big 2", "status": "legacy", "profile": "big" },
            { "slug": "big-1", "name": "Big 1", "status": "legacy", "profile": "big" }
        ]));
        let m = apply(&base(), &o);
        let cli = &m.catalogues["claude-cli"].models;
        let ids: Vec<_> = cli.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["big-3", "big-2", "small-1"], "a successor takes a superseded first row's place; a retired model nobody listed is not news");
        assert_eq!(cli[0].family.as_deref(), Some("big"), "the line of the model filed beside it");
        assert_eq!(cli[0].options, vec!["ladder"], "and its knobs");
        assert_eq!(cli[0].min_version.as_deref(), Some("1.4.0"));
        assert!(cli[1].legacy, "and what they superseded is superseded here");
        assert!(cli[0].pricing.is_none(), "a price is never borrowed");

        let api = &m.catalogues["anthropic"].models;
        assert!(api.iter().any(|e| e.id == "big-3" && e.min_version.is_none()), "the API has no CLI floor");
    }

    #[test]
    fn news_does_not_move_a_first_row_that_is_still_current() {
        let o = theirs(serde_json::json!([
            { "slug": "big-3", "name": "Big 3", "status": "current", "profile": "big" },
            { "slug": "big-4", "name": "Big 4", "status": "current", "profile": "big" },
            { "slug": "big-2", "name": "Big 2", "status": "current", "profile": "big" }
        ]));
        let cli = &apply(&base(), &o).catalogues["claude-cli"].models;
        let ids: Vec<_> = cli.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["big-2", "big-3", "big-4", "small-1"], "and two placed together keep their order");
    }

    #[test]
    fn a_floor_we_wrote_down_is_kept_and_one_we_did_not_is_taken() {
        let o = theirs(serde_json::json!([
            { "slug": "big-2", "name": "Big 2", "status": "current", "adapter": { "claudeCode": { "minVersion": "9.9.9" } } },
            { "slug": "small-1", "name": "Small 1", "status": "current", "adapter": { "claudeCode": { "minVersion": "1.0.5" } } }
        ]));
        let cli = &apply(&base(), &o).catalogues["claude-cli"].models;
        assert_eq!(cli[0].min_version.as_deref(), Some("1.2.0"));
        assert_eq!(cli[1].min_version.as_deref(), Some("1.0.5"));
    }

    #[test]
    fn a_model_with_no_sibling_gets_its_knobs_translated_and_no_rung() {
        let o = theirs(serde_json::json!([
            { "slug": "odd-1", "name": "Odd 1", "status": "current", "profile": "tiny" }
        ]));
        let m = apply(&base(), &o);
        let odd = m.catalogues["claude-cli"].models.iter().find(|e| e.id == "odd-1").expect("added");
        assert!(odd.tier.is_none(), "no rung nobody chose");
        assert_eq!(odd.context, Some(500_000));
        let knobs: Vec<_> = odd.options.iter().map(|n| m.knobs[n].id().to_string()).collect();
        assert_eq!(knobs, vec!["effort", "fast_mode"], "and only the knobs we can send");
        let ProviderOptionDescriptor::Select { options, prompt_injected_values, current_value, .. } =
            &m.knobs[&odd.options[0]]
        else {
            panic!("a ladder");
        };
        assert_eq!(current_value.as_deref(), Some("low"));
        assert_eq!(prompt_injected_values, &["ultrathink".to_string()]);
        assert_eq!(options.last().and_then(|o| o.description.as_deref()), Some("Past the top."));

        let api = m.catalogues["anthropic"].models.iter().find(|e| e.id == "odd-1").expect("added");
        let ProviderOptionDescriptor::Select { prompt_injected_values, .. } = &m.knobs[&api.options[0]] else {
            panic!("a ladder");
        };
        assert!(prompt_injected_values.is_empty(), "the API reads no words");
    }

    #[test]
    fn a_file_with_nothing_we_drive_is_refused() {
        assert!(parse("{}").is_err());
        assert!(parse(r#"{"updatedAt":"2026-01-01T00:00:00Z","providers":{"someone":{"models":[]}}}"#).is_err());
        assert!(parse(r#"{"updatedAt":"soon","providers":{}}"#).is_err());
    }

    #[test]
    fn the_bundled_snapshot_is_one_we_can_read_and_lay_over_ours() {
        let o = parse(crate::manifest::BUNDLED_T3CODE).expect("the snapshot parses");
        let base = crate::manifest::parse(crate::manifest::BUNDLED).expect("ours parses");
        let m = apply(&base, &o);
        crate::manifest::parse(&serde_json::to_string(&m).expect("serialise"))
            .expect("and what comes out is still a valid catalogue");
    }
}
