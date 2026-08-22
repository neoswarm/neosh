//! What is left of the plan, and what the plan has already been spent on.
//!
//! Two different questions, kept apart because they have different answers and different costs.
//!
//! **Quota** is *now*: how much of a rolling allowance has been used, and when it comes back. It is
//! the number that changes what you do in the next five minutes — at 94% of a weekly window the
//! useful move is to switch to a cheaper model or stop, and finding that out from a refusal is
//! finding out too late. Nobody can compute it locally: it belongs to the account, is enforced by
//! the vendor, and is only ever *reported*. See [`QuotaSnapshot`].
//!
//! **Usage** is *history*: tokens and their money-equivalent, bucketed by hour or day, read back
//! out of the vendor CLIs' own transcripts. It answers "where did the week go", which is a
//! different question from "may I send this", and it is derived rather than reported. See
//! [`UsageBucket`].
//!
//! They are deliberately not one type. A percentage of an opaque allowance cannot be added to a
//! token count, and a panel that implied it could would be lying in the direction that makes people
//! plan around a number that means nothing.
//!
//! # A window is whatever the vendor says it is
//!
//! [`QuotaWindow::id`] is the vendor's own name for a limit, not one of ours. Anthropic's
//! `/api/oauth/usage` answers with a `limits` array whose entries are `session`, `weekly_all` and
//! `weekly_scoped`; codex reports `primary` and `secondary`; a plugin's provider may report
//! something nobody here has heard of. Mapping those onto a fixed enum would mean a limit that
//! actually binds — the per-model weekly cap that is the reason a turn just failed — being dropped
//! on the floor for not being in our list. So the id is a string, the label is what to draw, and
//! anything with a percentage gets a row.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::agent::Usage;
use crate::provider::InstanceId;

/// How worried to be, as the vendor grades it.
///
/// Graded at the source rather than by a threshold here, because the thresholds are not ours to
/// pick: Anthropic calls 94% of a weekly window `critical` and 55% of a session window `normal`,
/// and a client that decided at 80% for both would be shouting about the wrong one. A source that
/// reports no grade gets one from [`QuotaSeverity::of`], which is the fallback and says so.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum QuotaSeverity {
    #[default]
    Normal,
    Warn,
    Critical,
    /// Nothing left. Distinct from `Critical` because it is the difference between "slow down" and
    /// "this will be refused".
    Exhausted,
}

impl QuotaSeverity {
    /// The grade a source that does not grade its own limits would have given.
    pub fn of(used_percent: f32) -> Self {
        if used_percent >= 100.0 {
            Self::Exhausted
        } else if used_percent >= 90.0 {
            Self::Critical
        } else if used_percent >= 75.0 {
            Self::Warn
        } else {
            Self::Normal
        }
    }

    /// The name of the highlight group a row at this grade wears.
    ///
    /// Named here rather than in each frontend because it is the same answer everywhere and the
    /// palette owns the colour: these are group names, not colours. `Diagnostic.Error` for
    /// exhausted is the same group the destructive answer of a confirmation wears, deliberately —
    /// it is the same claim, that the next thing you press does not do what you want.
    pub fn group(self) -> &'static str {
        match self {
            Self::Normal => "Comment",
            Self::Warn => "Meter.Warn",
            Self::Critical => "Meter.Full",
            Self::Exhausted => "Diagnostic.Error",
        }
    }
}

/// One rolling allowance, as the vendor describes it.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct QuotaWindow {
    /// The vendor's own name for this limit. Stable enough to key a redraw on, opaque by design.
    pub id: String,
    /// What to draw. Written for a person: `Session`, `Weekly`, `Weekly · Opus`.
    pub label: String,
    /// 0-100. Above 100 is possible and is not clamped — an account in overage really has used more
    /// than its allowance, and rounding that down to "full" loses the only interesting part.
    pub used_percent: f32,
    /// Unix seconds. `None` for an allowance that does not roll over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub resets_at: Option<i64>,
    /// How long the window is, when the vendor says. Lets a client show how fast it is being spent
    /// rather than only how much is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "number | null")]
    pub window_minutes: Option<u32>,
    #[serde(default)]
    pub severity: QuotaSeverity,
    /// What this limit is *about*, when it is about one thing: a model, a surface. `Fable`, `Opus`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// The limit currently doing the limiting. Several windows are usually open at once and only
    /// one of them is the reason a request would be refused.
    #[serde(default)]
    pub active: bool,
}

/// Paid-for headroom past the plan, where the vendor sells it.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[ts(export)]
pub struct QuotaCredits {
    /// Whether the account can actually spend past its plan. `false` is the interesting case: it is
    /// why hitting a limit stops work rather than costing money.
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f32>,
    /// Already formatted by whoever knew the currency. A balance rendered from a minor-unit integer
    /// by a client that guessed `USD` is a number that is wrong twice a year.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<String>,
    /// Why it is off, in the vendor's words, when it is off for a reason worth repeating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

/// Where a snapshot came from, which is the same thing as how much to trust its age.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum QuotaSource {
    /// A driver said so while a turn was running. Exact at the moment it arrived, and then only as
    /// fresh as the last turn.
    Turn,
    /// Asked, at rest. What makes a gauge move while you are reading rather than typing.
    Poll,
    /// A plugin reported it for a provider it registered. Nothing here knows how it was obtained,
    /// which is the point.
    Plugin,
}

/// Everything known about one account's allowance, as of one instant.
///
/// Whole rather than patched, for the reason [`crate::Activity::Plan`] is whole: a patch is only
/// correct if the previous state was, and every source of these reports the complete set anyway.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct QuotaSnapshot {
    /// Which configured provider instance this is the account of. Two instances may share one
    /// account and two accounts may share one vendor, so this is keyed by instance and never by
    /// vendor.
    pub instance: InstanceId,
    /// The plan's own name, when the vendor says it: `max`, `pro`, `plus`. Drawn as context for the
    /// percentages, never used to infer what the limits are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<QuotaCredits>,
    /// Unix seconds. The reason this is on the wire rather than stamped on arrival: a snapshot
    /// travels — over ASCP from another computer, out of a store on reattach — and "when it was
    /// received here" is a different fact from "when it was true".
    #[ts(type = "number")]
    pub observed_at: i64,
    pub source: QuotaSource,
}

impl QuotaSnapshot {
    /// The window a person means when they ask how much is left: the binding one.
    ///
    /// The vendor's own `active` flag first, then the fullest. Not simply the fullest, because a
    /// scoped limit at 100% that is not being enforced on this model is not the answer to "may I
    /// send this" — and it is exactly the row that would win a plain maximum.
    pub fn binding(&self) -> Option<&QuotaWindow> {
        self.windows
            .iter()
            .find(|w| w.active)
            .or_else(|| {
                self.windows.iter().max_by(|a, b| {
                    a.used_percent.partial_cmp(&b.used_percent).unwrap_or(std::cmp::Ordering::Equal)
                })
            })
    }

    /// The worst grade anything here carries, which is what a one-row summary is coloured by.
    pub fn severity(&self) -> QuotaSeverity {
        self.windows.iter().map(|w| w.severity).max_by_key(|s| *s as u8).unwrap_or_default()
    }
}

/// One window's percentage at one instant, kept so a gauge can be a line rather than a number.
///
/// Sampled rather than derived. Nothing on disk records what a plan percentage was an hour ago —
/// the vendor reports only *now* — so a history exists at all only because something wrote each
/// answer down as it arrived.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct QuotaSample {
    #[ts(type = "number")]
    pub at: i64,
    pub instance: InstanceId,
    /// [`QuotaWindow::id`], so a sample can be matched back to the window it belongs to.
    pub window: String,
    pub used_percent: f32,
}

/// How coarsely a history is bucketed.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum UsageResolution {
    Hour,
    Day,
}

impl UsageResolution {
    pub fn seconds(self) -> i64 {
        match self {
            Self::Hour => 3_600,
            Self::Day => 86_400,
        }
    }
}

/// Why a bucket's cost is what it is, so a client can be honest about it.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum CostBasis {
    /// A price was known for this model, and the cost is those tokens at those rates.
    Priced,
    /// Tokens are known and rates are not. Counted in the token totals and excluded from cost,
    /// rather than silently costed at zero — which reads as "this was free".
    #[default]
    Unpriced,
}

/// One `(bucket, instance, model)` cell of history.
///
/// The cost is what these tokens *would* have cost at API rates. It is not money spent: a
/// subscription bills separately and a plan turn costs nothing extra at all. It is here because it
/// is the only common unit that lets Opus and Haiku appear on one axis.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct UsageBucket {
    /// Unix seconds at the start of the bucket, in the zone the request asked for.
    #[ts(type = "number")]
    pub start: i64,
    pub instance: InstanceId,
    pub model: String,
    pub usage: Usage,
    pub cost_usd: f64,
    /// What the cached input would have cost at full input rates, minus what it did cost. The
    /// difference between a conversation that is expensive and one that only looks it.
    pub cache_savings_usd: f64,
    pub basis: CostBasis,
    /// Distinct assistant responses, after de-duplication.
    #[ts(type = "number")]
    pub requests: u32,
}

/// What one transcript directory yielded, so a client can say what it could not read.
///
/// A history with a hole in it and a history that is genuinely empty look identical in the buckets,
/// and the difference matters: one means "you did not use it", the other means "we could not tell".
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct UsageScanSource {
    pub instance: InstanceId,
    /// The directory that was read, for a message that names a real path.
    pub path: String,
    pub status: UsageScanStatus,
    #[ts(type = "number")]
    pub files: u32,
    /// Lines that parsed but carried nothing recognisable.
    #[ts(type = "number")]
    pub malformed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum UsageScanStatus {
    Ok,
    /// The directory is not there, which for a CLI that has never run is the normal answer.
    Missing,
    /// Some of it was read and some of it was not.
    Partial,
    Failed,
}

/// A whole answer to "where did the week go".
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Debug)]
#[ts(export)]
pub struct UsageHistory {
    pub buckets: Vec<UsageBucket>,
    pub sources: Vec<UsageScanSource>,
    pub resolution: UsageResolution,
    #[ts(type = "number")]
    pub since: i64,
    #[ts(type = "number")]
    pub until: i64,
    /// Whether every model that appeared had a price. Drawn as a caveat on the cost axis.
    pub fully_priced: bool,
    /// Wall-clock cost of the scan, so a slow answer can be explained rather than guessed at.
    #[ts(type = "number")]
    pub scanned_ms: u64,
}
