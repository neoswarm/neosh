//! What the plan has left, kept between the moments anything says so.
//!
//! The problem this exists for: every source of a plan percentage reports it at a moment that is
//! not the moment you want to know. `claude` says so on a line in the middle of a turn; `codex`
//! pushes a notification when the number moves. The question — *have I got enough left to start
//! this* — is asked with nothing running at all. So the answers have to be **kept**, which is the
//! same rule ADR 0036 states for anything a viewer needs on reattach: a value that is only
//! forwarded is a value that comes back wrong.
//!
//! # Patch or replace, decided by where it came from
//!
//! The two are not interchangeable and getting it backwards is silently wrong in both directions.
//!
//! - [`QuotaSource::Turn`] **patches**. `claude`'s `rate_limit_event` carries exactly one window —
//!   whichever is closest to binding — and says nothing whatever about the others. Treating it as a
//!   snapshot would delete the weekly row every time the session row moved.
//! - [`QuotaSource::Poll`] and [`QuotaSource::Plugin`] **replace**. Both are complete by
//!   construction, and only a replace can express a limit that has *gone* — a scoped weekly cap
//!   that expired, an overage that was switched off. Patched, those would sit there forever at
//!   whatever they last read.
//!
//! # The history is sampled, because nothing else records it
//!
//! There is no file anywhere that says what a plan percentage was an hour ago. The vendor reports
//! *now*, and a chart of the last five hours exists only because something wrote each answer down
//! as it arrived. That is what [`QuotaStore::history`] is, and why it is the one thing here that
//! touches disk.
//!
//! Sampled on change rather than on a timer: a percentage that has not moved is a horizontal line,
//! and a point every thirty seconds saying so is a file that grows forever to describe nothing. The
//! gaps are meaningful and are left as gaps — a client that drew a straight line across the
//! fourteen hours the machine was asleep would be inventing usage that did not happen.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use neosh_proto::quota::{QuotaSample, QuotaSnapshot, QuotaSource, QuotaWindow};
use neosh_proto::InstanceId;
use neosh_provider::quota::{self as vendor, Pollable, Quota};
use tokio::sync::mpsc;

/// How long a sample is kept.
///
/// Longer than the longest window anybody meters — a weekly limit — so that a chart of "this reset
/// period" always has the whole period in it, with room for the one before to compare against.
const RETENTION: i64 = 30 * 86_400;

/// A percentage has to move by this much to be worth a point.
///
/// The vendors report whole percents, so anything above zero would do; this is stated as a float
/// because a plugin-supplied source may report finer, and one that jitters in the third decimal
/// would otherwise write a sample per push.
const SAMPLE_EPSILON: f32 = 0.5;

/// The most samples held in memory and on disk.
///
/// A backstop, not a policy: retention is by age. This is what stops a pathological source that
/// reports a different percentage every second from filling the state directory before the next
/// prune gets a chance to run.
const SAMPLE_LIMIT: usize = 50_000;

pub struct QuotaStore {
    latest: HashMap<InstanceId, QuotaSnapshot>,
    /// Where the snapshots themselves are kept between runs. `None` under `--clean`.
    latest_path: Option<PathBuf>,
    samples: Vec<QuotaSample>,
    /// `None` under `--clean`, where the history lives for the life of the process and nowhere
    /// afterwards. Recording is an append; the file is only rewritten whole when a prune actually
    /// removes something, which is what keeps writing a sample down to one `write`.
    path: Option<PathBuf>,
}

impl QuotaStore {
    /// `None` for the state directory under `--clean`, where nothing is read and nothing written.
    ///
    /// The snapshots are read back, not only the samples. A percentage is only ever *reported*, so
    /// a workspace that forgot the last one starts every session with empty gauges and stays that
    /// way until something reports again — which on a machine that is offline, signed out, or
    /// being told to ask less often is indefinitely. Kept, it opens with the last thing anybody
    /// knew and says how old it is, which is a true statement and a useful one. Same rule as
    /// [`Editor::republish`]: a value that is only forwarded is a value that comes back wrong.
    pub fn new(state_dir: Option<&std::path::Path>) -> Self {
        let path = state_dir.map(|d| d.join("quota.jsonl"));
        let latest_path = state_dir.map(|d| d.join("quota.json"));
        let samples = path.as_deref().map(read_samples).unwrap_or_default();
        let latest = latest_path.as_deref().map(read_latest).unwrap_or_default();
        Self { latest, latest_path, samples, path }
    }

    /// Everything known, newest observation first.
    pub fn list(&self) -> Vec<QuotaSnapshot> {
        let mut out: Vec<QuotaSnapshot> = self.latest.values().cloned().collect();
        out.sort_by(|a, b| b.observed_at.cmp(&a.observed_at).then(a.instance.0.cmp(&b.instance.0)));
        out
    }

    /// The samples in a span, oldest first.
    pub fn history(&self, instance: Option<&InstanceId>, since: i64, until: i64) -> Vec<QuotaSample> {
        self.samples
            .iter()
            .filter(|s| s.at >= since && s.at < until)
            .filter(|s| instance.is_none_or(|i| &s.instance == i))
            .cloned()
            .collect()
    }

    /// Take in what a source said. Answers with the new snapshot when anything actually changed.
    ///
    /// `None` for a report that says the same as the last one, which is most of them: `codex`
    /// pushes on every turn boundary whether the number moved or not, and a redraw per push is a
    /// sidebar that flickers while nothing happens.
    pub fn apply(
        &mut self,
        instance: InstanceId,
        source: QuotaSource,
        quota: Quota,
        now: i64,
    ) -> Option<QuotaSnapshot> {
        if quota.is_empty() {
            return None;
        }
        let previous = self.latest.get(&instance);
        let windows = match source {
            QuotaSource::Turn => patch(previous.map(|p| p.windows.as_slice()), quota.windows),
            QuotaSource::Poll | QuotaSource::Plugin => quota.windows,
        };
        let snapshot = QuotaSnapshot {
            instance: instance.clone(),
            // A source that does not name the plan must not erase one that did. `claude`'s
            // mid-turn line never names it and the poll always does, so without this the plan
            // label would blink out every time a turn touched a limit.
            plan: quota.plan.or_else(|| previous.and_then(|p| p.plan.clone())),
            windows,
            credits: quota.credits.or_else(|| previous.and_then(|p| p.credits.clone())),
            observed_at: now,
            source,
        };

        let moved = previous.is_none_or(|p| changed(p, &snapshot));
        self.record(&snapshot, now);
        self.latest.insert(instance, snapshot.clone());
        // Written on every report rather than only on a change, because `observed_at` moves even
        // when the percentages do not — and the age is half of what a restored gauge says.
        self.persist_latest();
        moved.then_some(snapshot)
    }

    /// The snapshots, as one small file replaced whole.
    ///
    /// Whole rather than appended: there is one row per instance and it is superseded rather than
    /// added to, so an append-only log of them would be a file that grows forever to describe two
    /// accounts. Through a temporary, so a crash mid-write leaves the previous answer rather than
    /// half of this one.
    fn persist_latest(&self) {
        let Some(path) = self.latest_path.as_ref() else { return };
        let mut all: Vec<&QuotaSnapshot> = self.latest.values().collect();
        all.sort_by(|a, b| a.instance.0.cmp(&b.instance.0));
        let Ok(body) = serde_json::to_string_pretty(&all) else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Write down anything that has moved far enough to be a point on a line.
    fn record(&mut self, snapshot: &QuotaSnapshot, now: i64) {
        let mut fresh = Vec::new();
        for w in &snapshot.windows {
            let last = self
                .samples
                .iter()
                .rev()
                .find(|s| s.instance == snapshot.instance && s.window == w.id);
            if last.is_some_and(|s| (s.used_percent - w.used_percent).abs() < SAMPLE_EPSILON) {
                continue;
            }
            fresh.push(QuotaSample {
                at: now,
                instance: snapshot.instance.clone(),
                window: w.id.clone(),
                used_percent: w.used_percent,
            });
        }
        if fresh.is_empty() {
            return;
        }
        self.append(&fresh);
        self.samples.extend(fresh);
        self.prune(now);
    }

    fn prune(&mut self, now: i64) {
        let cutoff = now - RETENTION;
        let before = self.samples.len();
        self.samples.retain(|s| s.at >= cutoff);
        if self.samples.len() > SAMPLE_LIMIT {
            self.samples.drain(..self.samples.len() - SAMPLE_LIMIT);
        }
        // Only a prune that removed something is worth rewriting the file for. Everything else is
        // an append, which is why recording a sample costs one `write` and not a full serialise.
        if self.samples.len() != before {
            self.rewrite();
        }
    }

    fn append(&mut self, fresh: &[QuotaSample]) {
        let Some(path) = self.path.clone() else { return };
        let mut body = String::new();
        for s in fresh {
            match serde_json::to_string(s) {
                Ok(line) => {
                    body.push_str(&line);
                    body.push('\n');
                }
                Err(e) => tracing::debug!("quota sample would not serialise: {e}"),
            }
        }
        if body.is_empty() {
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        use std::io::Write;
        match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(body.as_bytes()) {
                    tracing::debug!("could not append to {}: {e}", path.display());
                }
            }
            Err(e) => tracing::debug!("could not open {}: {e}", path.display()),
        }
    }

    fn rewrite(&mut self) {
        let Some(path) = self.path.clone() else { return };
        let mut body = String::new();
        for s in &self.samples {
            if let Ok(line) = serde_json::to_string(s) {
                body.push_str(&line);
                body.push('\n');
            }
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Through a temporary, because the alternative to an atomic replace here is a truncated
        // file: the history is rewritten while the workspace is running, and a crash halfway
        // through a plain write leaves half a line that nothing can parse.
        let tmp = path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Existing windows, with the reported ones written over the top by id.
///
/// Order is the previous snapshot's, because it is the order the poll gave — the vendor's own —
/// and a sidebar strip whose rows swapped places whenever a mid-turn line arrived would be
/// unreadable for the exact seconds you are watching it.
fn patch(previous: Option<&[QuotaWindow]>, reported: Vec<QuotaWindow>) -> Vec<QuotaWindow> {
    let Some(previous) = previous else { return reported };
    let mut incoming: HashMap<String, QuotaWindow> =
        reported.into_iter().map(|w| (w.id.clone(), w)).collect();
    let mut out: Vec<QuotaWindow> = previous
        .iter()
        .map(|old| match incoming.remove(&old.id) {
            Some(new) => QuotaWindow {
                // A patch's `active` is only meaningful for the window it carries. Copying its
                // `false` onto the others is right; copying its `true` while leaving the previous
                // holder's `true` alone would leave two windows claiming to be the binding one,
                // which is why every window not in the patch is cleared below.
                scope: new.scope.clone().or_else(|| old.scope.clone()),
                window_minutes: new.window_minutes.or(old.window_minutes),
                ..new
            },
            None => QuotaWindow { active: false, ..old.clone() },
        })
        .collect();
    // A limit the previous snapshot had never heard of — a scoped weekly cap that has just started
    // applying. Appended rather than dropped: it is very often the one that matters.
    let mut fresh: Vec<QuotaWindow> = incoming.into_values().collect();
    fresh.sort_by(|a, b| a.id.cmp(&b.id));
    out.extend(fresh);
    out
}

/// Whether two snapshots differ in any way worth a redraw.
///
/// Deliberately not `PartialEq` on the whole struct: `observed_at` differs on every single report
/// and `source` differs whenever a poll follows a turn, and neither is a thing anybody draws.
fn changed(a: &QuotaSnapshot, b: &QuotaSnapshot) -> bool {
    a.plan != b.plan || a.credits != b.credits || a.windows != b.windows
}

fn read_latest(path: &std::path::Path) -> HashMap<InstanceId, QuotaSnapshot> {
    let Ok(body) = std::fs::read_to_string(path) else { return HashMap::new() };
    // A file this build cannot parse is a file written by a different one. Starting empty is the
    // right answer to that and it costs one poll; refusing to start would not be.
    let all: Vec<QuotaSnapshot> = serde_json::from_str(&body).unwrap_or_default();
    all.into_iter().map(|s| (s.instance.clone(), s)).collect()
}

fn read_samples(path: &std::path::Path) -> Vec<QuotaSample> {
    let Ok(body) = std::fs::read_to_string(path) else { return Vec::new() };
    // A line that will not parse is skipped rather than fatal. This file is appended to while the
    // machine is running and a power cut leaves a partial last line; losing one point is not a
    // reason to lose a month.
    body.lines().filter_map(|l| serde_json::from_str::<QuotaSample>(l).ok()).collect()
}

/* -------------------------------------------------------------------------- */
/* Polling                                                                    */
/* -------------------------------------------------------------------------- */

/// An instance that can be asked, and the program that answers for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub instance: InstanceId,
    pub which: Pollable,
    pub program: String,
}

/// What a poll produced, on its way back to the host loop.
pub struct Polled {
    pub instance: InstanceId,
    /// `None` when the vendor was asked and could not answer. Reported rather than dropped so the
    /// caller can slow down: the failure that matters is being told to ask less often, and a poller
    /// that keeps its cadence through one of those is the thing causing it.
    pub quota: Option<Quota>,
    /// Whether the vendor specifically said to back off, as opposed to merely not answering.
    pub throttled: bool,
}

/// Ask every target, concurrently, and send back whatever answers.
///
/// Concurrent because the two are unrelated and one of them spawns a process: run in sequence, a
/// machine with a `codex` that takes three seconds to start would make the Claude gauge three
/// seconds later than it needed to be, every time.
///
/// A target that cannot answer is silent rather than an error. Not being signed in to one of two
/// vendors is the ordinary case, and a notification about it on a timer is the fastest way to teach
/// somebody to turn a feature off.
pub async fn poll(targets: Vec<Target>, out: mpsc::UnboundedSender<Polled>) {
    let tasks: Vec<_> = targets
        .into_iter()
        .map(|t| {
            let out = out.clone();
            tokio::spawn(async move {
                if !vendor::can_poll(t.which, &t.program).await {
                    return;
                }
                let result = match t.which {
                    Pollable::Claude => vendor::claude_usage(ANTHROPIC_API).await,
                    Pollable::Codex => vendor::codex_usage(&t.program).await,
                };
                let polled = match result {
                    Ok(Some(quota)) => {
                        Polled { instance: t.instance, quota: Some(quota), throttled: false }
                    }
                    // Asked, answered, nothing to report: an account on a key rather than a plan.
                    // Not a failure and not worth backing off for.
                    Ok(None) => return,
                    Err(e) => {
                        let throttled = matches!(e, neosh_provider::ProviderError::RateLimited);
                        tracing::debug!(instance = %t.instance, "quota poll failed: {e}");
                        Polled { instance: t.instance, quota: None, throttled }
                    }
                };
                let _ = out.send(polled);
            })
        })
        .collect();
    for task in tasks {
        let _ = task.await;
    }
}

const ANTHROPIC_API: &str = "https://api.anthropic.com";

/// How often to ask when nothing is happening.
///
/// Five minutes, which is the resolution the numbers themselves have: a session window is five
/// hours, so one poll in five minutes is a hundred points across it — far more than a strip eight
/// cells wide can show. Asking more often would be spending somebody's network to redraw the same
/// glyph.
pub const IDLE_INTERVAL: Duration = Duration::from_secs(300);

/// The longest a backoff will stretch to.
///
/// An hour, because the thing being backed off from is a limit that resets in hours: asking again
/// sooner than that cannot learn anything new about the situation that caused it, and asking later
/// would mean a workspace left open all day never noticing the limit came back.
pub const MAX_BACKOFF: Duration = Duration::from_secs(3_600);

/// And how soon after a turn ends.
///
/// A turn is the only thing that moves these numbers by a visible amount, so the poll that matters
/// is the one just after one. Short, but not immediate: the vendor's own accounting lags the end of
/// the stream by a second or two, and asking at zero reliably returns the figure from before the
/// turn — which reads as a turn that cost nothing.
pub const AFTER_TURN: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::quota::QuotaSeverity;

    fn window(id: &str, pct: f32) -> QuotaWindow {
        QuotaWindow {
            id: id.into(),
            label: id.into(),
            used_percent: pct,
            resets_at: None,
            window_minutes: None,
            severity: QuotaSeverity::of(pct),
            scope: None,
            active: false,
        }
    }

    fn store() -> QuotaStore {
        QuotaStore { latest: HashMap::new(), latest_path: None, samples: Vec::new(), path: None }
    }

    fn quota(windows: Vec<QuotaWindow>) -> Quota {
        Quota { plan: None, windows, credits: None }
    }

    /// The bug this whole split exists to prevent: `claude` reports one window mid-turn, and a
    /// replace would delete every other row.
    #[test]
    fn a_mid_turn_report_patches_and_does_not_erase_the_others() {
        let mut s = store();
        let i = InstanceId::from("claude-cli");
        s.apply(
            i.clone(),
            QuotaSource::Poll,
            quota(vec![window("session", 20.0), window("weekly", 80.0)]),
            1_000,
        );
        let after = s
            .apply(i, QuotaSource::Turn, quota(vec![window("weekly", 91.0)]), 1_100)
            .expect("the weekly figure moved");
        let ids: Vec<&str> = after.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["session", "weekly"], "and in the order the poll gave");
        assert_eq!(after.windows[0].used_percent, 20.0, "untouched");
        assert_eq!(after.windows[1].used_percent, 91.0);
    }

    /// The other direction: a poll is complete, so a limit that has gone must actually go.
    #[test]
    fn a_poll_replaces_so_an_expired_limit_disappears() {
        let mut s = store();
        let i = InstanceId::from("claude-cli");
        s.apply(
            i.clone(),
            QuotaSource::Poll,
            quota(vec![window("session", 20.0), window("weekly_scoped", 100.0)]),
            1_000,
        );
        let after = s
            .apply(i, QuotaSource::Poll, quota(vec![window("session", 22.0)]), 2_000)
            .expect("the scoped cap expired");
        assert_eq!(after.windows.len(), 1);
        assert_eq!(after.windows[0].id, "session");
    }

    /// A limit that has just started applying is the one you most need to see.
    #[test]
    fn a_patch_can_introduce_a_window_nobody_had_seen() {
        let mut s = store();
        let i = InstanceId::from("claude-cli");
        s.apply(i.clone(), QuotaSource::Poll, quota(vec![window("session", 20.0)]), 1_000);
        let after = s
            .apply(i, QuotaSource::Turn, quota(vec![window("weekly_scoped", 100.0)]), 1_100)
            .expect("a new cap");
        let ids: Vec<&str> = after.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["session", "weekly_scoped"]);
    }

    /// Two windows both claiming to bind is a strip with two highlighted rows and no answer to
    /// "which one stops me".
    #[test]
    fn only_one_window_is_ever_the_binding_one() {
        let mut s = store();
        let i = InstanceId::from("claude-cli");
        let mut first = window("session", 20.0);
        first.active = true;
        s.apply(
            i.clone(),
            QuotaSource::Poll,
            quota(vec![first, window("weekly", 80.0)]),
            1_000,
        );
        let mut moved = window("weekly", 95.0);
        moved.active = true;
        let after = s.apply(i, QuotaSource::Turn, quota(vec![moved]), 1_100).unwrap();
        assert_eq!(after.windows.iter().filter(|w| w.active).count(), 1);
        assert!(after.windows[1].active);
    }

    /// `codex` pushes on every turn boundary whether the number moved or not.
    #[test]
    fn a_report_that_says_nothing_new_is_not_a_redraw() {
        let mut s = store();
        let i = InstanceId::from("codex-cli");
        assert!(s.apply(i.clone(), QuotaSource::Poll, quota(vec![window("primary", 5.0)]), 1).is_some());
        assert!(s.apply(i, QuotaSource::Poll, quota(vec![window("primary", 5.0)]), 2).is_none());
    }

    /// The mid-turn line never names the plan, and the label must not blink out when one arrives.
    #[test]
    fn a_source_that_does_not_know_the_plan_does_not_erase_it() {
        let mut s = store();
        let i = InstanceId::from("claude-cli");
        s.apply(
            i.clone(),
            QuotaSource::Poll,
            Quota { plan: Some("max".into()), windows: vec![window("session", 1.0)], credits: None },
            1_000,
        );
        let after = s
            .apply(i, QuotaSource::Turn, quota(vec![window("session", 9.0)]), 1_100)
            .unwrap();
        assert_eq!(after.plan.as_deref(), Some("max"));
    }

    /// A successful call with nothing in it must not replace a real snapshot with a gauge of zero.
    #[test]
    fn an_empty_report_is_ignored_outright() {
        let mut s = store();
        let i = InstanceId::from("claude-cli");
        s.apply(i.clone(), QuotaSource::Poll, quota(vec![window("session", 40.0)]), 1_000);
        assert!(s.apply(i.clone(), QuotaSource::Poll, quota(Vec::new()), 2_000).is_none());
        assert_eq!(s.list()[0].windows[0].used_percent, 40.0);
    }

    /// A flat line does not need a point every time somebody says it is still flat.
    #[test]
    fn samples_are_only_written_when_the_number_moves() {
        let mut s = store();
        let i = InstanceId::from("claude-cli");
        s.apply(i.clone(), QuotaSource::Poll, quota(vec![window("session", 10.0)]), 1_000);
        s.apply(i.clone(), QuotaSource::Poll, quota(vec![window("session", 10.0)]), 2_000);
        s.apply(i.clone(), QuotaSource::Poll, quota(vec![window("session", 11.0)]), 3_000);
        let h = s.history(Some(&i), 0, 10_000);
        assert_eq!(h.len(), 2, "the repeat is not a point");
        assert_eq!(h[0].at, 1_000);
        assert_eq!(h[1].at, 3_000);
    }

    #[test]
    fn history_is_bounded_by_the_span_and_the_instance_asked_for() {
        let mut s = store();
        let a = InstanceId::from("claude-cli");
        let b = InstanceId::from("codex-cli");
        s.apply(a.clone(), QuotaSource::Poll, quota(vec![window("session", 1.0)]), 1_000);
        s.apply(b.clone(), QuotaSource::Poll, quota(vec![window("primary", 2.0)]), 1_500);
        s.apply(a.clone(), QuotaSource::Poll, quota(vec![window("session", 3.0)]), 5_000);
        assert_eq!(s.history(None, 0, 10_000).len(), 3);
        assert_eq!(s.history(Some(&a), 0, 10_000).len(), 2);
        assert_eq!(s.history(Some(&a), 0, 2_000).len(), 1);
        // `until` is exclusive.
        assert_eq!(s.history(Some(&a), 1_000, 5_000).len(), 1);
    }

    /// A month is what a weekly window plus the one before it needs.
    #[test]
    fn samples_older_than_the_retention_are_dropped() {
        let mut s = store();
        let i = InstanceId::from("claude-cli");
        s.apply(i.clone(), QuotaSource::Poll, quota(vec![window("session", 1.0)]), 0);
        s.apply(i.clone(), QuotaSource::Poll, quota(vec![window("session", 2.0)]), RETENTION + 10);
        let h = s.history(None, i64::MIN / 2, i64::MAX / 2);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].used_percent, 2.0);
    }

    /// The list is newest-observation-first, so a strip that draws only the top row draws the one
    /// that was true most recently.
    #[test]
    fn the_list_leads_with_the_freshest() {
        let mut s = store();
        s.apply(InstanceId::from("a"), QuotaSource::Poll, quota(vec![window("w", 1.0)]), 100);
        s.apply(InstanceId::from("b"), QuotaSource::Poll, quota(vec![window("w", 2.0)]), 200);
        assert_eq!(s.list()[0].instance.0, "b");
    }

    /// The gauges survive a restart, which is the whole reason they are written down.
    ///
    /// A percentage is only ever *reported*. A workspace that forgot the last one opens with empty
    /// gauges and stays that way until something reports again — which on a machine that is
    /// offline, signed out, or being told to ask less often is indefinitely, and is exactly the
    /// state somebody would most want the last known figure in.
    #[test]
    fn the_last_known_snapshot_is_read_back() {
        let dir = std::env::temp_dir().join(format!("neosh-quota-latest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");

        {
            let mut s = QuotaStore::new(Some(&dir));
            s.apply(
                InstanceId::from("claude-cli"),
                QuotaSource::Poll,
                Quota {
                    plan: Some("max".into()),
                    windows: vec![window("weekly", 94.0)],
                    credits: None,
                },
                1_000,
            );
        }

        let reopened = QuotaStore::new(Some(&dir));
        let all = reopened.list();
        assert_eq!(all.len(), 1, "the snapshot came back");
        assert_eq!(all[0].plan.as_deref(), Some("max"));
        assert_eq!(all[0].windows[0].used_percent, 94.0);
        assert_eq!(all[0].observed_at, 1_000, "with the age it actually had, not with now");
        assert_eq!(reopened.history(None, 0, 10_000).len(), 1, "and the samples with it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file from a build that spelled these differently is not a reason to refuse to start.
    #[test]
    fn an_unreadable_snapshot_file_starts_empty_rather_than_failing() {
        let dir = std::env::temp_dir().join(format!("neosh-quota-junk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("quota.json"), "{\"not\": \"a list\"}").expect("write");
        assert!(QuotaStore::new(Some(&dir)).list().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A power cut leaves a half-written last line. Losing one point is not losing a month.
    #[test]
    fn a_truncated_history_file_still_loads_what_is_readable() {
        let dir = std::env::temp_dir().join(format!("neosh-quota-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("quota.jsonl");
        let good = serde_json::to_string(&QuotaSample {
            at: 5,
            instance: InstanceId::from("claude-cli"),
            window: "session".into(),
            used_percent: 7.0,
        })
        .unwrap();
        std::fs::write(&path, format!("{good}\n{{\"at\":6,\"inst")).unwrap();
        let loaded = read_samples(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].used_percent, 7.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
