//! The model catalogue, as data that can move between releases.
//!
//! # Two files, two jobs
//!
//! `models.json` beside this crate's `Cargo.toml` is **ours**: every catalogue this workspace
//! knows, with the knobs our drivers read, prices, taglines, the ladder `model.upgrade` climbs and
//! the CLI floors we read off the CLIs' own refusals. It is compiled in and changes with the code.
//!
//! What it cannot do is keep up. A vendor ships a model on a Tuesday, and a list bound to a release
//! says nothing about it until somebody edits it, cuts a release and every machine installs one. So
//! the *news* comes from somebody whose job is keeping up anyway: t3code's model manifest
//! ([`crate::t3code`]), which describes the same three CLIs we drive and is updated the day a model
//! ships. A snapshot of it is compiled in beside ours (`t3code-models.json`, refreshed at release
//! by `scripts/sync-models.sh`), the workspace fetches theirs from GitHub (`neosh::models`), and
//! the catalogue in use is always ours with theirs laid over it.
//!
//! # Which of theirs wins
//!
//! Their `updatedAt`: a copy is put in use when it is at least as new as the one in use and says
//! something different. Fetch time cannot say which is newer — an upgraded binary bundles a
//! snapshot the cache on disk predates — and their stamp is the only clock both copies share. Ours
//! is never in that comparison: it is the base every copy of theirs is laid over, so editing
//! `models.json` takes effect on the next build whatever GitHub says.
//!
//! # What a fetched copy may not do
//!
//! Break the picker. Theirs is read defensively and refused whole when nothing in it is usable, and
//! ours is checked here (`parse`) — a name pointing at no knob would silently drop a rung, a
//! duplicate id would make two rows one selection.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use neosh_proto::{ModelCapabilities, ModelInfo, ModelTier, Pricing, ProviderOptionDescriptor};
use serde::{Deserialize, Serialize};

use crate::t3code::{self, Overlay};

/// Our catalogue, compiled in.
pub const BUNDLED: &str = include_str!("../models.json");

/// The snapshot of t3code's manifest this binary was released with.
pub const BUNDLED_T3CODE: &str = include_str!("../t3code-models.json");

/// The newest `version` this binary can read. Raise it only for a change an older reader would
/// get *wrong* — a new optional field is not one, because unknown fields are ignored.
pub const SUPPORTED: u32 = 1;

/// The whole file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    /// Named option descriptors a model lists in `options`. Named once because ten models of one
    /// generation share one ladder, and ten copies of it are ten places to forget a rung.
    #[serde(default)]
    pub knobs: BTreeMap<String, ProviderOptionDescriptor>,
    /// Keyed by provider instance id — `claude-cli`, `openai`, `antigravity`.
    pub catalogues: BTreeMap<String, Catalogue>,
}

/// One provider's models.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalogue {
    /// Whether the endpoint caches prompts. A fact about the transport, so said once.
    #[serde(default)]
    pub prompt_caching: bool,
    /// Newest first: the first live row is what a fresh install lands on.
    pub models: Vec<Entry>,
    /// Which knobs a model *nobody wrote down* gets, by id prefix, first match wins — so one
    /// discovered this morning arrives with its ladder rather than with none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<ModelTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tagline: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<Pricing>,
    /// Names in [`Manifest::knobs`], in the order `^E` draws them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Whether it reasons, where that is not what its knobs say: a `thinking` switch on a model
    /// that does not reason by default, a `mode` knob on one that always does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    /// The oldest vendor CLI that accepts it, `2.1.251`. A model is announced the day it ships and
    /// the row says which update unlocks it, rather than the model being absent until it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub prefix: String,
    /// Empty is an answer — "no knobs" — and stops the search, which is how `gemini-2.5-pro` is
    /// kept off the `gemini-2.5` switch it would reject.
    #[serde(default)]
    pub options: Vec<String>,
}

/// Whether a knob is about *reasoning* rather than about the shape of the answer.
///
/// One list, because three vendors spell the same idea three ways and `ModelCapabilities::thinking`
/// is a single bit that a picker uses to decide whether a model reasons at all.
pub fn is_reasoning(id: &str) -> bool {
    matches!(id, "effort" | "thinking" | "thinking_level" | "thinking_budget")
}

impl Manifest {
    fn knobs_named(&self, names: &[String]) -> Vec<ProviderOptionDescriptor> {
        names.iter().filter_map(|n| self.knobs.get(n).cloned()).collect()
    }

    /// One catalogue's models, described.
    pub fn models(&self, catalogue: &str) -> Option<Vec<ModelInfo>> {
        let c = self.catalogues.get(catalogue)?;
        Some(c.models.iter().map(|e| self.describe(c, e)).collect())
    }

    fn describe(&self, c: &Catalogue, e: &Entry) -> ModelInfo {
        let descriptors = self.knobs_named(&e.options);
        let mut m = ModelInfo::undescribed(e.id.as_str(), e.name.as_str());
        m.context_window = e.context;
        m.max_output_tokens = e.max_output;
        m.capabilities = ModelCapabilities {
            tools: true,
            vision: true,
            streaming: true,
            thinking: e
                .thinking
                .unwrap_or_else(|| descriptors.iter().any(|d| is_reasoning(d.id()))),
            prompt_caching: c.prompt_caching,
            option_descriptors: descriptors,
        };
        m.pricing = e.pricing;
        m.family = e.family.clone();
        m.tier = e.tier;
        m.tagline = e.tagline.clone();
        m.legacy = e.legacy;
        m
    }

    /// The knobs a model id gets from the catalogue's rules, for an id nobody wrote down.
    pub fn knobs_for(&self, catalogue: &str, id: &str) -> Option<Vec<ProviderOptionDescriptor>> {
        let c = self.catalogues.get(catalogue)?;
        let rule = c.rules.iter().find(|r| id.starts_with(r.prefix.as_str()))?;
        Some(self.knobs_named(&rule.options))
    }

    /// The oldest CLI version a model needs, if one is written down.
    pub fn min_version(&self, catalogue: &str, id: &str) -> Option<(u32, u32, u32)> {
        let c = self.catalogues.get(catalogue)?;
        let e = c.models.iter().find(|e| e.id == id)?;
        e.min_version.as_deref().and_then(parse_version)
    }

    /// Every model id in every catalogue, as `catalogue/id`. What changed between two manifests is
    /// the difference between two of these.
    pub fn ids(&self) -> Vec<String> {
        self.catalogues
            .iter()
            .flat_map(|(c, cat)| cat.models.iter().map(move |e| format!("{c}/{}", e.id)))
            .collect()
    }
}

/// `2.1.251` -> `(2, 1, 251)`. A missing patch is zero; anything else that is not a number is no
/// version at all.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let mut parts = s.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// Read a manifest and check it is one this binary can use.
///
/// Everything a broken file could do to the picker is checked here rather than discovered there:
/// a name pointing at no knob would silently drop a rung, a duplicate id would make two rows one
/// selection, an empty catalogue would make a provider look like it serves nothing.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let m: Manifest = serde_json::from_str(text).map_err(|e| format!("not a model manifest: {e}"))?;
    if m.version > SUPPORTED {
        return Err(format!(
            "written for a newer neosh (manifest version {}, this reads {SUPPORTED})",
            m.version
        ));
    }
    if m.catalogues.values().all(|c| c.models.is_empty()) {
        return Err("lists no models at all".into());
    }
    for (name, c) in &m.catalogues {
        let mut seen = std::collections::BTreeSet::new();
        for e in &c.models {
            if e.id.trim().is_empty() || e.name.trim().is_empty() {
                return Err(format!("{name}: a model with no id or no name"));
            }
            if !seen.insert(e.id.as_str()) {
                return Err(format!("{name}: {} is listed twice", e.id));
            }
            if let Some(missing) = e.options.iter().find(|o| !m.knobs.contains_key(*o)) {
                return Err(format!("{name}/{}: no knob called {missing:?}", e.id));
            }
            if let Some(v) = &e.min_version
                && parse_version(v).is_none()
            {
                return Err(format!("{name}/{}: min_version {v:?} is not a version", e.id));
            }
        }
        for r in &c.rules {
            if let Some(missing) = r.options.iter().find(|o| !m.knobs.contains_key(*o)) {
                return Err(format!("{name}: rule {:?} names no knob called {missing:?}", r.prefix));
            }
        }
    }
    Ok(m)
}

/// Seconds since the epoch for `YYYY-MM-DDTHH:MM:SSZ`, and nothing for any other spelling — one
/// spelling is what makes the newer of two the greater string.
pub fn timestamp_secs(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = s.get(r)?;
        part.bytes().all(|c| c.is_ascii_digit()).then(|| part.parse().ok())?
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm), so no date crate is needed for one
    // comparison against the clock.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3_600 + mi * 60 + sec)
}

fn base() -> &'static Manifest {
    static BASE: OnceLock<Manifest> = OnceLock::new();
    // A broken bundle is a build that should never have shipped, and `manifest::tests` is what
    // stops it; an empty catalogue keeps the binary running if one ever does.
    BASE.get_or_init(|| {
        parse(BUNDLED).unwrap_or_else(|e| {
            tracing::error!("the bundled model manifest is broken: {e}");
            Manifest { version: SUPPORTED, knobs: BTreeMap::new(), catalogues: BTreeMap::new() }
        })
    })
}

fn bundled_news() -> Option<&'static Overlay> {
    static NEWS: OnceLock<Option<Overlay>> = OnceLock::new();
    NEWS.get_or_init(|| {
        t3code::parse(BUNDLED_T3CODE)
            .map_err(|e| tracing::error!("the bundled t3code snapshot is unreadable: {e}"))
            .ok()
    })
    .as_ref()
}

/// The copy of theirs in use, and the catalogue it makes. Absent until one newer than the bundled
/// snapshot is offered.
struct InUse {
    news: Arc<Overlay>,
    manifest: Arc<Manifest>,
}

static IN_USE: RwLock<Option<InUse>> = RwLock::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn from_bundle() -> &'static Arc<Manifest> {
    static BUNDLE: OnceLock<Arc<Manifest>> = OnceLock::new();
    BUNDLE.get_or_init(|| {
        Arc::new(match bundled_news() {
            Some(news) => t3code::apply(base(), news),
            None => base().clone(),
        })
    })
}

/// The catalogue in use: ours, with the newest copy of theirs laid over it.
pub fn current() -> Arc<Manifest> {
    let held = IN_USE.read().ok().and_then(|c| c.as_ref().map(|u| u.manifest.clone()));
    held.unwrap_or_else(|| from_bundle().clone())
}

/// How many times the catalogue in use has changed. Compared, not read: a reader that saw `3` and
/// now sees `4` has a catalogue to rebuild.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Put a copy of t3code's manifest in use if it is no older than the one in use and says something
/// different. Answers whether it was.
///
/// A tie on the stamp with different contents is taken: their file is edited by hand and a stamp
/// left alone across an edit is a stamp, not a statement that nothing changed.
pub fn offer(news: Overlay) -> bool {
    let Ok(mut held) = IN_USE.write() else { return false };
    let stale = {
        let older = |n: &Overlay| news.updated_at < n.updated_at || *n == news;
        match held.as_ref() {
            Some(u) => older(&u.news),
            None => bundled_news().is_some_and(older),
        }
    };
    if stale {
        return false;
    }
    let manifest = Arc::new(t3code::apply(base(), &news));
    *held = Some(InUse { news: Arc::new(news), manifest });
    GENERATION.fetch_add(1, Ordering::AcqRel);
    true
}

/// One catalogue's models from the catalogue in use.
pub fn models(catalogue: &str) -> Vec<ModelInfo> {
    current().models(catalogue).unwrap_or_default()
}

/// [`Manifest::knobs_for`] on the catalogue in use.
pub fn knobs_for(catalogue: &str, id: &str) -> Vec<ProviderOptionDescriptor> {
    current().knobs_for(catalogue, id).unwrap_or_default()
}

/// [`Manifest::min_version`] on the catalogue in use.
pub fn min_version(catalogue: &str, id: &str) -> Option<(u32, u32, u32)> {
    current().min_version(catalogue, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny(ids: &[&str]) -> String {
        let models: Vec<_> = ids
            .iter()
            .map(|id| serde_json::json!({ "id": id, "name": id.to_uppercase(), "options": ["effort"] }))
            .collect();
        serde_json::json!({
            "version": 1,
            "knobs": { "effort": {
                "type": "select", "id": "effort", "label": "Effort",
                "options": [{ "id": "low", "label": "Low" }, { "id": "high", "label": "High", "is_default": true }]
            }},
            "catalogues": { "synthetic": { "models": models,
                "rules": [{ "prefix": "syn-pro", "options": [] }, { "prefix": "syn", "options": ["effort"] }] } }
        })
        .to_string()
    }

    #[test]
    fn the_bundled_manifest_is_one_this_binary_can_read() {
        let m = parse(BUNDLED).expect("models.json parses and validates");
        assert!(m.version <= SUPPORTED);
        assert!(!m.catalogues.is_empty());
    }

    #[test]
    fn a_model_gets_the_knobs_it_names_and_reasons_if_one_of_them_is_reasoning() {
        let m = parse(&tiny(&["syn-1"])).expect("valid");
        let got = m.models("synthetic").expect("catalogue");
        assert_eq!(got[0].display_name, "SYN-1");
        assert_eq!(got[0].capabilities.option_descriptors.len(), 1);
        assert!(got[0].capabilities.thinking);
    }

    #[test]
    fn a_rule_gives_an_unwritten_model_its_ladder_and_an_empty_rule_stops_the_search() {
        let m = parse(&tiny(&["syn-1"])).expect("valid");
        assert_eq!(m.knobs_for("synthetic", "syn-9").map(|k| k.len()), Some(1));
        assert_eq!(m.knobs_for("synthetic", "syn-pro-2").map(|k| k.len()), Some(0));
        assert_eq!(m.knobs_for("synthetic", "other"), None);
    }

    #[test]
    fn a_broken_copy_is_refused_with_a_reason() {
        assert!(parse("{").is_err());
        let newer = tiny(&["a"]).replace("\"version\":1", "\"version\":99");
        assert!(parse(&newer).unwrap_err().contains("newer neosh"));
        let twice = tiny(&["a", "a"]);
        assert!(parse(&twice).unwrap_err().contains("twice"));
        let dangling = tiny(&["a"]).replace("\"options\":[\"effort\"]", "\"options\":[\"nope\"]");
        assert!(parse(&dangling).unwrap_err().contains("nope"));
    }

    #[test]
    fn dates_compare_as_dates() {
        assert_eq!(timestamp_secs("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(timestamp_secs("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert!(timestamp_secs("2026-13-01T00:00:00Z").is_none());
        assert!(timestamp_secs("2026-01-01 00:00:00Z").is_none());
    }

    #[test]
    fn versions_read_like_versions() {
        assert_eq!(parse_version("2.1.251"), Some((2, 1, 251)));
        assert_eq!(parse_version("2.1"), Some((2, 1, 0)));
        assert_eq!(parse_version("2.x.1"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
    }
}
