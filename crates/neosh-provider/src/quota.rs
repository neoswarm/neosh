//! Where a plan's remaining allowance comes from.
//!
//! Three sources, and they are not interchangeable:
//!
//! 1. **A driver mid-turn.** `claude` emits a `rate_limit_event` line and `codex app-server` an
//!    `account/rateLimits/updated` notification. Free, instant, and only while something is
//!    running — which is never the moment the question is asked.
//! 2. **Asking, at rest.** [`claude_usage`] is one authenticated GET; [`codex_usage`] is a
//!    short-lived `app-server` and one request. Neither spends a token of the allowance it reports,
//!    which is the only reason polling one is defensible at all.
//! 3. **A plugin**, for a provider it registered. Nothing here is involved.
//!
//! # Reading another program's credential
//!
//! [`claude_usage`] reads `~/.claude/.credentials.json`, which belongs to the `claude` CLI. That is
//! the same trade this crate already makes everywhere else: the `claude-cli` driver exists because
//! reusing a login you already have is what makes neosh useful the minute it is installed, and a
//! plan gauge that demanded a second sign-in for a number the first login already entitles you to
//! would not be worth the ceremony.
//!
//! The rules do not bend for it. The token is a [`SecretString`], read at request time and dropped
//! with the request; it is never written anywhere, never logged, and structurally absent from every
//! type in [`neosh_proto::quota`]. What crosses a boundary is percentages. And it is a *setting* —
//! `usage.poll` — so a machine where reaching into another program's state directory is the wrong
//! answer turns it off and gets the mid-turn numbers instead.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use neosh_proto::quota::{QuotaCredits, QuotaSeverity, QuotaWindow};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

use crate::ProviderError;

/// What a vendor was asked and what it said, minus the credential.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Quota {
    pub plan: Option<String>,
    pub windows: Vec<QuotaWindow>,
    pub credits: Option<QuotaCredits>,
}

impl Quota {
    /// Whether there is anything here worth keeping.
    ///
    /// An authenticated call that answers with every limit `null` — which is what an API-key
    /// account gets — is a successful request with no allowance to report. Storing it would replace
    /// a real snapshot with an empty one and draw a gauge reading zero.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty() && self.credits.is_none()
    }
}

/* -------------------------------------------------------------------------- */
/* Anthropic                                                                  */
/* -------------------------------------------------------------------------- */

/// Where `claude` keeps its OAuth tokens on a machine with no keychain.
fn claude_credentials_path() -> Option<PathBuf> {
    let home = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs_home().map(|h| h.join(".claude")))?;
    Some(home.join(".credentials.json"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// The access token, if there is a live subscription login on this machine.
///
/// Deliberately not refreshed. A token that has expired means the `claude` CLI will mint a new one
/// the next time it runs, and racing it to the refresh endpoint is how two programs invalidate each
/// other's session. An expired token is simply "cannot answer right now", which is a state a gauge
/// showing its own age already knows how to draw.
async fn claude_token() -> Option<(SecretString, Option<String>)> {
    let path = claude_credentials_path()?;
    let raw = tokio::fs::read_to_string(&path).await.ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let oauth = v.get("claudeAiOauth")?;

    // Milliseconds, and a minute of slack so a token about to die is not used for a request that
    // will outlive it.
    let expires_at = oauth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0);
    if expires_at > 0 && expires_at < (now_ms() + 60_000) {
        return None;
    }

    let token = oauth.get("accessToken").and_then(Value::as_str)?;
    if token.is_empty() {
        return None;
    }
    let plan = oauth.get("subscriptionType").and_then(Value::as_str).map(str::to_string);
    Some((SecretString::from(token.to_string()), plan))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Ask Anthropic what is left, using the login `claude` already has.
///
/// `Ok(None)` for "nobody is signed in here", which is not an error and must not be drawn as one:
/// an account on an API key has no plan to have a percentage of, and a machine that has never run
/// `claude` is the normal case rather than a broken one.
pub async fn claude_usage(base_url: &str) -> Result<Option<Quota>, ProviderError> {
    let Some((token, plan)) = claude_token().await else { return Ok(None) };

    let resp = crate::drivers::http::client()
        .get(format!("{}/api/oauth/usage", base_url.trim_end_matches('/')))
        .header("Authorization", format!("Bearer {}", token.expose_secret()))
        .header("anthropic-beta", OAUTH_BETA)
        .header("Content-Type", "application/json")
        .send()
        .await
        .map_err(|e| ProviderError::Transport(e.to_string()))?;

    let status = resp.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        // The login is there and does not entitle us to ask — an account without `user:profile`,
        // or a token the CLI has since rotated. Same answer as no login: nothing to report.
        return Ok(None);
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // The endpoint that tells you about rate limits has one of its own, and asking about your
        // allowance costs some of a different allowance. Reported as a distinct error so the caller
        // can back off rather than keep the cadence that caused it — which is the failure mode
        // where a gauge you added to avoid hitting limits is the thing hitting one.
        return Err(ProviderError::RateLimited);
    }
    if !status.is_success() {
        return Err(ProviderError::BadResponse(format!("oauth usage: HTTP {status}")));
    }

    let body: Value = resp.json().await.map_err(|e| ProviderError::BadResponse(e.to_string()))?;
    let mut quota = parse_claude_usage(&body);
    if quota.plan.is_none() {
        quota.plan = plan;
    }
    Ok(if quota.is_empty() { None } else { Some(quota) })
}

/// Read the endpoint's answer.
///
/// The `limits` array is preferred and the named fields are the fallback, in that order, because
/// they are the same facts at different resolutions. `limits` is self-describing — it carries the
/// vendor's own severity, whether a limit is currently binding, and *what it is scoped to* — and it
/// is the only one of the two that can express the per-model weekly cap that is, in practice, the
/// limit people actually hit. `five_hour` and `seven_day` are what an older server answers with,
/// and dropping them would mean an account seeing nothing at all rather than seeing less.
pub fn parse_claude_usage(v: &Value) -> Quota {
    let mut windows = Vec::new();

    if let Some(limits) = v.get("limits").and_then(Value::as_array) {
        for l in limits {
            if let Some(w) = claude_limit(l) {
                windows.push(w);
            }
        }
    }
    if windows.is_empty() {
        for (key, label, minutes) in CLAUDE_LEGACY_WINDOWS {
            let Some(entry) = v.get(*key).filter(|e| !e.is_null()) else { continue };
            let Some(pct) = entry.get("utilization").and_then(Value::as_f64) else { continue };
            windows.push(QuotaWindow {
                id: claude_canonical(key).to_string(),
                label: (*label).to_string(),
                used_percent: pct as f32,
                resets_at: entry.get("resets_at").and_then(parse_time),
                window_minutes: Some(*minutes),
                severity: QuotaSeverity::of(pct as f32),
                scope: None,
                active: false,
            });
        }
        // Nothing said which one binds, so the fullest does. Only correct here: every window in
        // this shape applies to every request, so the fullest really is the one that refuses first.
        if let Some(i) = fullest(&windows) {
            windows[i].active = true;
        }
    }

    Quota { plan: None, windows, credits: claude_credits(v) }
}

/// The legacy named windows, with how long each one is.
///
/// The durations are in the names and are not reported anywhere in this shape, which is why they
/// are written down: without one, a client cannot say how fast an allowance is going, only how much
/// of it has gone.
const CLAUDE_LEGACY_WINDOWS: &[(&str, &str, u32)] = &[
    ("five_hour", "Session", 300),
    ("seven_day", "Weekly", 10_080),
    ("seven_day_opus", "Weekly · Opus", 10_080),
    ("seven_day_sonnet", "Weekly · Sonnet", 10_080),
];

fn fullest(windows: &[QuotaWindow]) -> Option<usize> {
    windows
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            a.used_percent.partial_cmp(&b.used_percent).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(i, _)| i)
}

/// One entry of the `limits` array.
fn claude_limit(l: &Value) -> Option<QuotaWindow> {
    let kind = l.get("kind").and_then(Value::as_str)?;
    let percent = l.get("percent").and_then(Value::as_f64)? as f32;
    // The model a scoped limit is about. The display name rather than the id: the id is frequently
    // null on these, and the name is the word the person would use anyway.
    let scope = l
        .pointer("/scope/model/display_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(QuotaWindow {
        label: claude_limit_label(kind, l.get("group").and_then(Value::as_str), scope.as_deref()),
        id: claude_canonical(kind).to_string(),
        used_percent: percent,
        resets_at: l.get("resets_at").and_then(parse_time),
        window_minutes: claude_window_minutes(kind, l.get("group").and_then(Value::as_str)),
        severity: l
            .get("severity")
            .and_then(Value::as_str)
            .map_or_else(|| QuotaSeverity::of(percent), claude_severity),
        scope,
        active: l.get("is_active").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// The name this build uses for a limit, given whichever of the vendor's two names it arrived as.
///
/// Anthropic spells the same limits two ways. The `limits` array says `session` / `weekly_all` /
/// `weekly_scoped`; a `rate_limit_event` line says `five_hour` / `seven_day` / `seven_day_opus`.
/// They are the same allowances, and the store patches a polled snapshot with a mid-turn event *by
/// id* — so leaving both spellings in would not produce two labels for one row, it would produce
/// two rows. The session limit would appear twice, at different percentages, and the fuller of the
/// two would win a gauge it should never have been in.
///
/// A kind nobody here recognises keeps its own spelling. The `limits` array grew three new kinds
/// between one release and the next, and a build that dropped what it had not been taught would
/// silently stop showing whichever one had started binding.
fn claude_canonical(kind: &str) -> &str {
    match kind {
        "five_hour" | "session" => "session",
        "seven_day" | "weekly_all" => "weekly",
        "seven_day_opus" => "weekly_opus",
        "seven_day_sonnet" => "weekly_sonnet",
        other => other,
    }
}

/// What to draw a limit as.
fn claude_limit_label(kind: &str, group: Option<&str>, scope: Option<&str>) -> String {
    let base = match claude_canonical(kind) {
        "session" => "Session".to_string(),
        k if k.starts_with("weekly") => match k.strip_prefix("weekly_") {
            // `weekly_scoped` is named by its scope, which is appended below; naming it twice
            // gives `Weekly · Scoped · Fable`.
            Some("scoped") | None => "Weekly".to_string(),
            Some(model) => format!("Weekly · {}", title_case(model)),
        },
        other => match group {
            Some("session") => "Session".to_string(),
            Some("weekly") => "Weekly".to_string(),
            _ => title_case(other),
        },
    };
    match scope {
        Some(model) => format!("{base} · {model}"),
        None => base,
    }
}

fn claude_window_minutes(kind: &str, group: Option<&str>) -> Option<u32> {
    match (claude_canonical(kind), group) {
        ("session", _) | (_, Some("session")) => Some(300),
        (k, _) if k.starts_with("weekly") => Some(10_080),
        (_, Some("weekly")) => Some(10_080),
        _ => None,
    }
}

fn claude_severity(s: &str) -> QuotaSeverity {
    match s {
        "warning" | "warn" | "elevated" => QuotaSeverity::Warn,
        "critical" => QuotaSeverity::Critical,
        "exhausted" | "reached" | "blocked" => QuotaSeverity::Exhausted,
        _ => QuotaSeverity::Normal,
    }
}

/// Extra usage, read from whichever of the two shapes is present.
///
/// `spend` is the newer one and carries a formatted balance; `extra_usage` is the older one and
/// carries a utilisation. The interesting bit in both is `enabled`: it is the difference between
/// running out meaning "this stops" and running out meaning "this starts costing money", and a
/// gauge that did not say which would be describing two very different situations identically.
fn claude_credits(v: &Value) -> Option<QuotaCredits> {
    if let Some(spend) = v.get("spend").filter(|s| !s.is_null()) {
        return Some(QuotaCredits {
            enabled: spend.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            used_percent: spend.get("percent").and_then(Value::as_f64).map(|p| p as f32),
            balance: money(spend.get("balance")),
            limit: money(spend.get("limit")),
            disabled_reason: spend
                .get("disabled_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    let extra = v.get("extra_usage").filter(|e| !e.is_null())?;
    Some(QuotaCredits {
        enabled: extra.get("is_enabled").and_then(Value::as_bool).unwrap_or(false),
        used_percent: extra.get("utilization").and_then(Value::as_f64).map(|p| p as f32),
        balance: None,
        limit: None,
        disabled_reason: extra.get("disabled_reason").and_then(Value::as_str).map(str::to_string),
    })
}

/// A minor-unit amount, rendered by the only code that knows its currency and exponent.
///
/// Formatted here rather than on the wire as three numbers because the alternative is every client
/// re-deriving it, and a client that assumed cents renders a yen figure a hundred times too small.
fn money(v: Option<&Value>) -> Option<String> {
    let v = v.filter(|v| !v.is_null())?;
    let minor = v.get("amount_minor").and_then(Value::as_i64)?;
    let exponent = v.get("exponent").and_then(Value::as_u64).unwrap_or(2) as u32;
    let currency = v.get("currency").and_then(Value::as_str).unwrap_or("USD");
    let divisor = 10f64.powi(exponent as i32);
    let symbol = match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        _ => "",
    };
    let amount = format!("{:.*}", exponent as usize, minor as f64 / divisor);
    Some(if symbol.is_empty() {
        format!("{amount} {currency}")
    } else {
        format!("{symbol}{amount}")
    })
}

/// How full a window has to be for `allowed_warning` to be taken at its word.
///
/// Seventy percent, which is the figure the vendor's own client uses for the same decision on the
/// same field. It is not a threshold of ours in the sense the doc on [`QuotaSeverity`] warns about
/// — it is not deciding *when to worry*, it is deciding whether the flag beside the number is
/// about the number or is left over from before the last reset.
const CLAUDE_WARNING_THRESHOLD: f64 = 70.0;

/// One `rate_limit_event` line of `claude --output-format stream-json`.
///
/// A much thinner shape than the endpoint's: one window, the one that is closest to binding, with
/// no scope and no idea what the others are at. So it *patches* rather than replaces — see
/// [`Quota::is_empty`] and the store that applies it. It is worth having anyway, because it is the
/// only figure that moves during the twenty minutes an agent driver can spend on one turn.
///
/// **Its `utilization` is a fraction, and the endpoint's is a percentage.** The same word, two
/// scales, one type away from each other: `SDKRateLimitInfoSchema` carries the number the CLI read
/// off an `anthropic-ratelimit-unified-*-utilization` response header, which is `0.56` for
/// fifty-six percent — the vendor multiplies by a hundred at every use of it and compares it to a
/// `0.7` threshold — while `/api/oauth/usage` answers with `percent` and a legacy `utilization`
/// that are both already out of a hundred. Read as a percentage, a weekly limit at 56% patched the
/// polled snapshot down to `0.56`, which is `1%` in the sidebar and `99% left` under it, and stayed
/// that way until the next poll replaced it — which is what made opening the panel look like the
/// thing that fixed it. It also meant the grade below was computed from a number that can never
/// reach [`QuotaSeverity::Warn`], so nothing mid-turn ever crossed a threshold on its own.
pub fn parse_claude_rate_limit_event(info: &Value) -> Quota {
    let Some(kind) = info.get("rateLimitType").and_then(Value::as_str) else {
        return Quota::default();
    };
    let Some(pct) = info.get("utilization").and_then(Value::as_f64).map(|u| u * 100.0) else {
        return Quota::default();
    };
    // `allowed_warning` is only worth believing once the number agrees with it. The vendor's own
    // client discards it below seventy percent and says why: the endpoint sends `allowed_warning`
    // with stale data for a while after a window resets, so an account four percent into a fresh
    // week gets told it is nearly out. Graded straight through, that is a red row and a
    // notification about a limit that is further away than it has been all week — and `Critical`
    // is the grade the strip spends its loudest colour on.
    let severity = match info.get("status").and_then(Value::as_str) {
        Some("rejected") => QuotaSeverity::Exhausted,
        Some("allowed_warning") if pct >= CLAUDE_WARNING_THRESHOLD => QuotaSeverity::Critical,
        _ => QuotaSeverity::of(pct as f32),
    };
    let window = QuotaWindow {
        id: claude_canonical(kind).to_string(),
        label: claude_limit_label(kind, None, None),
        used_percent: pct as f32,
        // Seconds here, unlike the endpoint's ISO string.
        resets_at: info.get("resetsAt").and_then(Value::as_i64),
        window_minutes: claude_window_minutes(kind, None),
        severity,
        scope: None,
        // The CLI reports the limit it is closest to, which is the definition of binding.
        active: true,
    };
    Quota { plan: None, windows: vec![window], credits: None }
}

/* -------------------------------------------------------------------------- */
/* Codex                                                                      */
/* -------------------------------------------------------------------------- */

/// An `account/rateLimits/updated` payload, or the result of `account/rateLimits/read`.
///
/// Two rolling windows with no names of their own — `primary` is the short one and `secondary` the
/// long one — so the labels come from the durations the vendor reports beside them. That is better
/// than "Primary"/"Secondary", which say nothing, and better than assuming 5h/weekly, which is a
/// guess that goes stale the first time a plan changes shape.
pub fn parse_codex_rate_limits(v: &Value) -> Quota {
    let mut windows = Vec::new();
    for (key, fallback) in [("primary", "Session"), ("secondary", "Weekly")] {
        let Some(w) = v.get(key).filter(|w| !w.is_null()) else { continue };
        let Some(pct) = w.get("usedPercent").and_then(Value::as_f64) else { continue };
        let minutes = w.get("windowDurationMins").and_then(Value::as_u64).map(|m| m as u32);
        windows.push(QuotaWindow {
            id: key.to_string(),
            label: minutes.map_or_else(|| fallback.to_string(), duration_label),
            used_percent: pct as f32,
            // Seconds.
            resets_at: w.get("resetsAt").and_then(Value::as_i64),
            window_minutes: minutes,
            severity: QuotaSeverity::of(pct as f32),
            scope: None,
            active: false,
        });
    }
    // `rateLimitReachedType` says one is already reached; otherwise the fullest binds. Both windows
    // apply to every request here, so the fullest really is the one that refuses first.
    let reached = v.get("rateLimitReachedType").and_then(Value::as_str).is_some();
    if let Some(i) = fullest(&windows) {
        windows[i].active = true;
        if reached {
            windows[i].severity = QuotaSeverity::Exhausted;
        }
    }

    let credits = v.get("credits").filter(|c| !c.is_null()).map(|c| QuotaCredits {
        enabled: c.get("hasCredits").and_then(Value::as_bool).unwrap_or(false)
            || c.get("unlimited").and_then(Value::as_bool).unwrap_or(false),
        used_percent: None,
        balance: c.get("balance").and_then(Value::as_str).map(str::to_string),
        limit: None,
        disabled_reason: None,
    });

    Quota {
        plan: v.get("planType").and_then(Value::as_str).map(str::to_string),
        windows,
        credits,
    }
}

/// A window named by how long it is: `5h`, `Weekly`, `30d`.
fn duration_label(minutes: u32) -> String {
    match minutes {
        0..=90 => format!("{minutes}m"),
        m if m % 10_080 == 0 && m / 10_080 == 1 => "Weekly".to_string(),
        m if m % 1_440 == 0 => format!("{}d", m / 1_440),
        m if m % 60 == 0 => format!("{}h", m / 60),
        m => format!("{}h{}m", m / 60, m % 60),
    }
}

/// Ask a short-lived `codex app-server` what is left.
///
/// The same shape as model discovery, and for the same reason: `app-server` is the only surface the
/// CLI has that answers questions, and `account/rateLimits/read` costs nothing but a process. It
/// answers `Ok(None)` when the account is not signed in — this server ignores requests it cannot
/// serve rather than returning an error, so the absence of a reply within the deadline is the
/// answer and not a failure to draw.
pub async fn codex_usage(program: &str) -> Result<Option<Quota>, ProviderError> {
    let mut child = Command::new(program)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ProviderError::Transport(format!("could not run {program} app-server: {e}")))?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut lines = BufReader::new(stdout).lines();

    let hello = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "clientInfo": { "name": "neosh", "title": "neosh", "version": env!("CARGO_PKG_VERSION") } },
    });
    let ready = serde_json::json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} });
    let ask = serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "account/rateLimits/read", "params": {},
    });
    for msg in [hello, ready, ask] {
        stdin
            .write_all(format!("{msg}\n").as_bytes())
            .await
            .map_err(|e| ProviderError::Transport(format!("writing to {program}: {e}")))?;
    }
    stdin.flush().await.ok();

    // Bounded, because "no reply" is a real outcome here rather than a hang: an app-server with no
    // login answers unknown and unauthorised requests with silence, and a poll that waited forever
    // for one would hold a process open for the life of the workspace.
    let read = async {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            // An unprompted push is as good an answer as the reply, and often arrives first.
            if v.get("method").and_then(Value::as_str) == Some("account/rateLimits/updated") {
                if let Some(p) = v.get("params") {
                    return Some(parse_codex_rate_limits(snapshot_of(p)));
                }
            }
            if v.get("id").and_then(Value::as_u64) != Some(2) {
                continue;
            }
            if v.get("error").is_some() {
                return None;
            }
            let result = v.get("result")?;
            return Some(parse_codex_rate_limits(snapshot_of(result)));
        }
        None
    };
    let quota = tokio::time::timeout(std::time::Duration::from_secs(20), read).await.ok().flatten();
    let _ = child.kill().await;

    Ok(quota.filter(|q| !q.is_empty()))
}

/// The snapshot inside whatever wrapper this message used.
///
/// The notification nests it under `rateLimits` and the reply does not, and both have been seen
/// with and without across versions — so this looks for the nesting rather than being told which
/// message it is reading.
fn snapshot_of(v: &Value) -> &Value {
    v.get("rateLimits").filter(|r| !r.is_null()).unwrap_or(v)
}

/* -------------------------------------------------------------------------- */

/// `weekly_scoped` -> `Weekly scoped`. For a limit kind nobody here has a name for.
fn title_case(s: &str) -> String {
    let spaced = s.replace(['_', '-'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// An RFC 3339 timestamp as unix seconds.
///
/// Hand-rolled because the only formats these endpoints emit are
/// `2026-08-21T17:29:59.914596+00:00` and `2026-08-21T17:29:59Z`, and a date crate for two shapes
/// is a dependency to keep in step forever. Anything it cannot read is `None`, which draws as a
/// percentage without a countdown rather than as a countdown to the wrong time.
fn parse_time(v: &Value) -> Option<i64> {
    // Some of these are already seconds.
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    let s = v.as_str()?;
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }

    let days = days_from_civil(y, mo, d);
    let mut out = days * 86_400 + h * 3_600 + mi * 60 + sec;

    // The offset, when it is not Z. `+05:30` means the wall clock is ahead of UTC, so UTC is behind
    // it — subtract. Getting this backwards puts a reset eleven hours in the past.
    let rest = &s[19..];
    let tz = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if !tz.is_empty() && !tz.starts_with('Z') && !tz.starts_with('z') {
        let sign = if tz.starts_with('-') { -1 } else { 1 };
        let body = &tz[1..];
        let (oh, om) = match body.split_once(':') {
            Some((h, m)) => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?),
            None if body.len() >= 4 => {
                (body[..2].parse::<i64>().ok()?, body[2..4].parse::<i64>().ok()?)
            }
            None => (body.parse::<i64>().ok()?, 0),
        };
        out -= sign * (oh * 3_600 + om * 60);
    }
    Some(out)
}

/// Days since the unix epoch, by Howard Hinnant's `days_from_civil`.
///
/// Exact for every proleptic Gregorian date, which a `365 * y + y / 4` approximation is not — and
/// the failure of the approximation is a reset time that is a day out, roughly once a century, in a
/// way no test written this century would catch.
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Whether this instance is one the host can ask, and how.
///
/// Keyed on the driver rather than the instance id, because a user may configure `claude-cli` under
/// any name they like and what decides how to ask is which program is behind it.
///
/// The CLI drivers only, and deliberately: the credential a poll uses is the *CLI's* login, so it
/// is an answer about the CLI's account. An `anthropic` instance on an API key has no plan to have
/// a percentage of, and polling it here would file the subscription's figures under a key that is
/// not billed that way at all — a gauge that is not merely stale but about somebody else.
pub fn pollable(driver: &str) -> Option<Pollable> {
    match driver {
        "claude-cli" | "claude-control" => Some(Pollable::Claude),
        "codex-cli" => Some(Pollable::Codex),
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pollable {
    Claude,
    Codex,
}

/// Whether there is anything on this machine that could answer for `which`.
///
/// Checked before spawning or connecting so that the common case — a machine with one of the two
/// CLIs installed — does not pay for the other one on every poll.
pub async fn can_poll(which: Pollable, program: &str) -> bool {
    match which {
        Pollable::Claude => match claude_credentials_path() {
            Some(p) => tokio::fs::try_exists(&p).await.unwrap_or(false),
            None => false,
        },
        Pollable::Codex => which_on_path(program).is_some(),
    }
}

fn which_on_path(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = Path::new(program);
        return p.is_file().then(|| p.to_path_buf());
    }
    std::env::var_os("PATH")?.to_str().and_then(|path| {
        path.split(':')
            .map(|dir| Path::new(dir).join(program))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from a real `GET /api/oauth/usage`, so this pins the contract rather than
    /// a remembered one. The interesting part is `weekly_scoped`: full, and the only one that is
    /// `is_active`, while the window a naive maximum would pick is a different row.
    const REAL: &str = r#"{
      "five_hour": {"utilization": 55.0, "resets_at": "2026-08-21T17:29:59.914596+00:00"},
      "seven_day": {"utilization": 94.0, "resets_at": "2026-08-22T00:59:59.914643+00:00"},
      "seven_day_opus": null,
      "extra_usage": {"is_enabled": false, "utilization": null},
      "limits": [
        {"kind":"session","group":"session","percent":55,"severity":"normal",
         "resets_at":"2026-08-21T17:29:59.914596+00:00","scope":null,"is_active":false},
        {"kind":"weekly_all","group":"weekly","percent":94,"severity":"critical",
         "resets_at":"2026-08-22T00:59:59.914643+00:00","scope":null,"is_active":false},
        {"kind":"weekly_scoped","group":"weekly","percent":100,"severity":"critical",
         "resets_at":"2026-08-22T00:59:59.914866+00:00",
         "scope":{"model":{"id":null,"display_name":"Fable"}},"is_active":true}
      ],
      "spend": {"used":{"amount_minor":0,"currency":"USD","exponent":2},"limit":null,
                "percent":0,"severity":"normal","enabled":false,
                "balance":{"amount_minor":250,"currency":"USD","exponent":2}}
    }"#;

    #[test]
    fn reads_the_limits_array_in_preference_to_the_named_fields() {
        let q = parse_claude_usage(&serde_json::from_str(REAL).unwrap());
        let ids: Vec<&str> = q.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["session", "weekly", "weekly_scoped"]);
        assert_eq!(q.windows[1].label, "Weekly");
        assert_eq!(q.windows[2].label, "Weekly · Fable");
        assert_eq!(q.windows[2].scope.as_deref(), Some("Fable"));
    }

    /// The vendor grades its own limits and we do not second-guess it: 55% is `normal` here even
    /// though a fixed threshold might call it something else.
    #[test]
    fn severity_comes_from_the_vendor() {
        let q = parse_claude_usage(&serde_json::from_str(REAL).unwrap());
        assert_eq!(q.windows[0].severity, QuotaSeverity::Normal);
        assert_eq!(q.windows[1].severity, QuotaSeverity::Critical);
    }

    /// The whole reason `binding()` prefers `active` over the maximum.
    #[test]
    fn the_binding_window_is_the_one_the_vendor_flagged() {
        let q = parse_claude_usage(&serde_json::from_str(REAL).unwrap());
        let active: Vec<&str> =
            q.windows.iter().filter(|w| w.active).map(|w| w.id.as_str()).collect();
        assert_eq!(active, ["weekly_scoped"]);
    }

    #[test]
    fn credits_come_from_spend_when_it_is_there() {
        let q = parse_claude_usage(&serde_json::from_str(REAL).unwrap());
        let c = q.credits.expect("spend was present");
        assert!(!c.enabled);
        assert_eq!(c.balance.as_deref(), Some("$2.50"));
    }

    /// An older server sends no `limits`, and the named fields have to carry it.
    #[test]
    fn falls_back_to_the_named_windows() {
        let v = serde_json::json!({
            "five_hour": {"utilization": 12.0, "resets_at": "2026-08-21T17:29:59Z"},
            "seven_day": {"utilization": 88.0, "resets_at": null},
            "seven_day_sonnet": null,
        });
        let q = parse_claude_usage(&v);
        let ids: Vec<&str> = q.windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["session", "weekly"]);
        assert_eq!(q.windows[0].window_minutes, Some(300));
        // Nothing said which binds, so the fullest does.
        assert!(q.windows[1].active);
        assert!(!q.windows[0].active);
    }

    /// A kind this build has never heard of still gets a row, rather than vanishing.
    #[test]
    fn an_unknown_limit_kind_is_still_drawn() {
        let v = serde_json::json!({
            "limits": [{"kind": "monthly_burst", "percent": 40, "is_active": true}],
        });
        let q = parse_claude_usage(&v);
        assert_eq!(q.windows.len(), 1);
        assert_eq!(q.windows[0].label, "Monthly burst");
        assert_eq!(q.windows[0].severity, QuotaSeverity::Normal);
    }

    /// An account with a key and no plan: a successful call with nothing to report. Storing this
    /// would replace a real snapshot with a gauge reading zero.
    #[test]
    fn an_answer_with_no_limits_is_empty_rather_than_zero() {
        let v = serde_json::json!({ "five_hour": null, "seven_day": null });
        assert!(parse_claude_usage(&v).is_empty());
    }

    #[test]
    fn a_rate_limit_event_carries_the_window_that_binds() {
        let info = serde_json::json!({
            "status": "allowed_warning", "rateLimitType": "seven_day",
            "utilization": 0.91, "resetsAt": 1_787_400_000_i64,
        });
        let q = parse_claude_rate_limit_event(&info);
        assert_eq!(q.windows.len(), 1);
        assert_eq!(q.windows[0].id, "weekly");
        assert_eq!(q.windows[0].label, "Weekly");
        assert!(q.windows[0].active);
        assert_eq!(q.windows[0].severity, QuotaSeverity::Critical);
        assert_eq!(q.windows[0].resets_at, Some(1_787_400_000));
    }

    /// The scale, on its own, because it is the whole bug and it is invisible in every other
    /// assertion here: a mid-turn line says `0.56` where the endpoint says `56`.
    #[test]
    fn a_rate_limit_events_utilization_is_a_fraction() {
        let info = serde_json::json!({ "rateLimitType": "seven_day", "utilization": 0.56 });
        let q = parse_claude_rate_limit_event(&info);
        assert_eq!(q.windows[0].used_percent, 56.0);
        // And the grade comes off the same number, so it moves with it. Read as a percentage this
        // was `Normal` at every value a fraction can take.
        assert_eq!(q.windows[0].severity, QuotaSeverity::Normal);

        let hot = serde_json::json!({ "rateLimitType": "five_hour", "utilization": 0.94 });
        let q = parse_claude_rate_limit_event(&hot);
        assert_eq!(q.windows[0].used_percent, 94.0);
        assert_eq!(q.windows[0].severity, QuotaSeverity::Critical);
    }

    /// `allowed_warning` arriving on a window that is nowhere near full, which is what the endpoint
    /// sends for a while after a reset. Graded on the flag alone this was a red row and a
    /// notification about the emptiest allowance of the week.
    #[test]
    fn a_stale_warning_flag_does_not_outrank_the_number_beside_it() {
        let stale = serde_json::json!({
            "status": "allowed_warning", "rateLimitType": "seven_day", "utilization": 0.04,
        });
        assert_eq!(parse_claude_rate_limit_event(&stale).windows[0].severity, QuotaSeverity::Normal);

        // At the threshold and above it, the flag is about the number and is believed — including
        // where the number on its own would only have been `Warn`.
        let real = serde_json::json!({
            "status": "allowed_warning", "rateLimitType": "seven_day", "utilization": 0.7,
        });
        assert_eq!(
            parse_claude_rate_limit_event(&real).windows[0].severity,
            QuotaSeverity::Critical,
        );

        // `rejected` is never second-guessed: it is not a prediction, it is a refusal that already
        // happened.
        let out = serde_json::json!({
            "status": "rejected", "rateLimitType": "seven_day", "utilization": 0.0,
        });
        assert_eq!(
            parse_claude_rate_limit_event(&out).windows[0].severity,
            QuotaSeverity::Exhausted,
        );
    }

    /// The other side of the same coin, and the reason the two spellings are not interchangeable:
    /// the endpoint's numbers are already out of a hundred, in both of its shapes.
    #[test]
    fn the_endpoints_percentages_are_left_alone() {
        let limits = serde_json::json!({
            "limits": [{ "kind": "weekly_all", "percent": 56.0 }],
        });
        assert_eq!(parse_claude_usage(&limits).windows[0].used_percent, 56.0);

        let legacy = serde_json::json!({ "seven_day": { "utilization": 56.0 } });
        assert_eq!(parse_claude_usage(&legacy).windows[0].used_percent, 56.0);
    }

    /// The two arriving one after the other, which is the sequence that was on screen: a poll, then
    /// a turn moving the window it happens to be about. Patched by id, so the fixed scale is what
    /// decides whether the row goes up by a point or falls off the bottom of the gauge.
    #[test]
    fn a_turn_patch_lands_on_the_same_scale_as_the_poll_it_patches() {
        let polled = parse_claude_usage(&serde_json::json!({
            "limits": [
                { "kind": "session", "percent": 9.0 },
                { "kind": "weekly_all", "percent": 56.0 },
            ],
        }));
        let event = parse_claude_rate_limit_event(
            &serde_json::json!({ "rateLimitType": "seven_day", "utilization": 0.57 }),
        );
        assert_eq!(polled.windows.len(), 2);
        assert_eq!(event.windows[0].id, polled.windows[1].id);
        assert!(
            event.windows[0].used_percent > polled.windows[1].used_percent,
            "a turn spends an allowance, so the patch has to read higher than the poll before it",
        );
    }

    /// `status: allowed` with no type at all, which is what the line looks like before anything has
    /// been spent. Nothing to patch with.
    #[test]
    fn a_bare_rate_limit_event_says_nothing() {
        let info = serde_json::json!({ "status": "allowed" });
        assert!(parse_claude_rate_limit_event(&info).is_empty());
    }

    #[test]
    fn codex_windows_are_named_by_their_durations() {
        let v = serde_json::json!({
            "planType": "plus",
            "primary": {"usedPercent": 20, "resetsAt": 1_787_400_000_i64, "windowDurationMins": 300},
            "secondary": {"usedPercent": 71, "resetsAt": 1_787_900_000_i64, "windowDurationMins": 10080},
        });
        let q = parse_codex_rate_limits(&v);
        assert_eq!(q.plan.as_deref(), Some("plus"));
        assert_eq!(q.windows[0].label, "5h");
        assert_eq!(q.windows[1].label, "Weekly");
        assert!(q.windows[1].active, "the fullest binds when nothing says otherwise");
    }

    #[test]
    fn a_reached_codex_limit_is_exhausted() {
        let v = serde_json::json!({
            "primary": {"usedPercent": 100, "windowDurationMins": 300},
            "rateLimitReachedType": "rate_limit_reached",
        });
        let q = parse_codex_rate_limits(&v);
        assert_eq!(q.windows[0].severity, QuotaSeverity::Exhausted);
    }

    /// The nesting differs between the push and the reply, and has differed between versions.
    #[test]
    fn the_snapshot_is_found_through_either_wrapper() {
        let inner = serde_json::json!({"primary": {"usedPercent": 5, "windowDurationMins": 60}});
        let wrapped = serde_json::json!({"rateLimits": inner.clone()});
        assert_eq!(parse_codex_rate_limits(snapshot_of(&wrapped)).windows.len(), 1);
        assert_eq!(parse_codex_rate_limits(snapshot_of(&inner)).windows.len(), 1);
    }

    #[test]
    fn timestamps_are_read_in_both_shapes_the_endpoints_emit() {
        // 2026-08-21T17:29:59Z
        let z = parse_time(&Value::String("2026-08-21T17:29:59Z".into())).unwrap();
        let frac =
            parse_time(&Value::String("2026-08-21T17:29:59.914596+00:00".into())).unwrap();
        assert_eq!(z, frac);
        // A real offset moves it the other way from the wall clock.
        let plus = parse_time(&Value::String("2026-08-21T17:29:59+05:30".into())).unwrap();
        assert_eq!(z - plus, 5 * 3_600 + 30 * 60);
    }

    #[test]
    fn the_epoch_is_where_the_civil_calendar_says_it_is() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(parse_time(&Value::String("1970-01-01T00:00:00Z".into())), Some(0));
    }

    #[test]
    fn a_window_is_named_by_how_long_it_is() {
        assert_eq!(duration_label(300), "5h");
        assert_eq!(duration_label(10_080), "Weekly");
        assert_eq!(duration_label(43_200), "30d");
        assert_eq!(duration_label(90), "90m");
    }
}
