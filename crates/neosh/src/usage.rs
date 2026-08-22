//! Where the week went.
//!
//! [`neosh_proto::quota`] answers "may I send this"; this answers "what have I been spending it
//! on". The two are separate for a reason worth restating here, because the temptation to join them
//! is constant: a plan percentage is an opaque fraction of an allowance the vendor will not
//! itemise, and a token count is a number. Neither converts into the other, and a chart that put
//! them on one axis would be inventing an exchange rate.
//!
//! # Read from the vendors' transcripts, not from ours
//!
//! Every source here is a file some *other* program wrote: `~/.claude/projects/**/*.jsonl`,
//! `~/.codex/sessions/**/*.jsonl`. neosh has its own record of every turn it drove and it is the
//! wrong thing to count, for one blunt reason — a turn run in `claude` directly, in another editor,
//! or by a script, spent the same weekly allowance, and a history that could not see it would
//! answer a question nobody asked. The vendor's own transcript is the only place all of it is.
//!
//! This is the approach `ccusage` takes and it is the right one.
//!
//! # Everything here is bounded
//!
//! A scan walks directories somebody else fills. Each of the three bounds below exists because
//! without it a stranger's disk decides how long a keypress takes:
//!
//! - files whose modification time is outside the span are not opened at all;
//! - a line is `JSON::parse`d only after a substring test says it might carry usage, which skips
//!   roughly half of a real transcript for the cost of a `memchr`;
//! - the whole scan is capped, and a scan that hits the cap says so in [`UsageScanSource`] rather
//!   than returning a short answer that looks complete.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use neosh_proto::quota::{
    CostBasis, UsageBucket, UsageHistory, UsageResolution, UsageScanSource, UsageScanStatus,
};
use neosh_proto::{InstanceId, ModelInfo, Pricing, Usage};

/// How many transcript files one scan will open.
///
/// A real `~/.claude/projects` after a year is a few thousand files; this is above that and below
/// the point where a scan is noticeable. The cap is reported, never silent — a truncated history
/// drawn without a caveat reads as "you did not use it much", which is the wrong lesson.
const FILE_LIMIT: usize = 4_000;

/// Bytes read from any one transcript.
///
/// A single very long session can be hundreds of megabytes of tool output. The usage records are
/// spread evenly through it, so a cap loses the tail of one file rather than the file.
const FILE_BYTE_LIMIT: u64 = 256 * 1024 * 1024;

/// A vendor whose transcripts can be read, and where they are.
struct Vendor {
    instance: InstanceId,
    root: PathBuf,
    kind: VendorKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VendorKind {
    Claude,
    Codex,
}

impl VendorKind {
    /// The substring gate applied before a line is parsed. See the module docs.
    fn gate(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["\"usage\""],
            // Three, because codex spreads one record across three lines: the tokens, the model
            // that produced them and whether this rollout is a copy of somebody else's. Gating on
            // the tokens alone would parse a third as many lines and attribute every one of them
            // to no model at all.
            Self::Codex => &["\"token_count\"", "\"turn_context\"", "\"session_meta\""],
        }
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// Where each vendor keeps its transcripts on this machine.
///
/// Both honour the environment variable their own CLI honours, so a machine that has moved its
/// `claude` home does not get a history that silently reads the wrong directory — or, worse, an
/// empty one that looks like a quiet week.
fn vendors() -> Vec<Vendor> {
    let mut out = Vec::new();
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".claude")));
    if let Some(root) = claude {
        out.push(Vendor {
            instance: InstanceId::from("claude-cli"),
            root: root.join("projects"),
            kind: VendorKind::Claude,
        });
    }
    let codex = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".codex")));
    if let Some(root) = codex {
        out.push(Vendor {
            instance: InstanceId::from("codex-cli"),
            root: root.join("sessions"),
            kind: VendorKind::Codex,
        });
    }
    out
}

/// One assistant response's worth of tokens.
struct Record {
    at: i64,
    model: String,
    usage: Usage,
    /// A key that identifies this response across files, or `None` when it is inherently unique.
    dedupe: Option<String>,
}

/// Scan every vendor's transcripts for the span, and bucket what is there.
///
/// Blocking. Called from `spawn_blocking`, because it is file I/O measured in thousands of files
/// and the host loop is the single writer for the editor.
pub fn history(
    since: i64,
    until: i64,
    resolution: UsageResolution,
    utc_offset: i64,
    prices: &HashMap<String, Pricing>,
) -> UsageHistory {
    let started = std::time::Instant::now();
    let mut cells: HashMap<(i64, InstanceId, String), Cell> = HashMap::new();
    let mut sources = Vec::new();
    let mut budget = FILE_LIMIT;

    for vendor in vendors() {
        let mut source = UsageScanSource {
            instance: vendor.instance.clone(),
            path: vendor.root.display().to_string(),
            status: UsageScanStatus::Ok,
            files: 0,
            malformed: 0,
            message: None,
        };
        if !vendor.root.is_dir() {
            source.status = UsageScanStatus::Missing;
            sources.push(source);
            continue;
        }

        let mut files = Vec::new();
        collect(&vendor.root, &mut files, &mut source);
        // Newest first, so that a scan which runs out of budget keeps the part of the history
        // anybody is looking at. Truncating from the recent end would leave a chart whose right
        // edge — the only part that answers "what have I just been doing" — is the missing part.
        files.sort_by_key(|(_, mtime)| std::cmp::Reverse(*mtime));

        // A record is stamped with when the *response* happened, which can be well after the file
        // was created and a little before it was last written. The window is widened by a day at
        // each end so a session still being appended to, and one that began the day before the
        // span, are both opened; the per-record filter below is what actually decides.
        let lower = since - 86_400;
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (path, mtime) in files {
            if mtime < lower {
                continue;
            }
            if budget == 0 {
                source.status = UsageScanStatus::Partial;
                source.message = Some(format!(
                    "stopped after {FILE_LIMIT} files; the oldest part of this span is missing"
                ));
                break;
            }
            budget -= 1;
            source.files += 1;

            let records = match read_file(&path, vendor.kind, &mut source.malformed) {
                Ok(r) => r,
                Err(e) => {
                    source.status = UsageScanStatus::Partial;
                    source.message.get_or_insert_with(|| format!("{}: {e}", path.display()));
                    continue;
                }
            };
            for r in records {
                if r.at < since || r.at >= until {
                    continue;
                }
                // A claude transcript writes one record per *content block*, each repeating the
                // whole message's usage. Summing them overcounts a real workload by well over
                // double, so the first wins and the repeats are dropped.
                if let Some(key) = &r.dedupe
                    && !seen.insert(key.clone())
                {
                    continue;
                }
                let start = bucket_start(r.at, resolution, utc_offset);
                let cell = cells
                    .entry((start, vendor.instance.clone(), r.model.clone()))
                    .or_default();
                cell.add(&r.usage);
            }
        }
        sources.push(source);
    }

    let mut buckets: Vec<UsageBucket> = cells
        .into_iter()
        .map(|((start, instance, model), cell)| {
            let price = lookup(prices, &model);
            let (cost, savings) = price.map_or((0.0, 0.0), |p| priced(&cell.usage, p));
            UsageBucket {
                start,
                instance,
                model,
                usage: cell.usage,
                cost_usd: cost,
                cache_savings_usd: savings,
                basis: if price.is_some() { CostBasis::Priced } else { CostBasis::Unpriced },
                requests: cell.requests,
            }
        })
        .collect();
    // Sorted on the way out so every client draws the same chart without re-sorting: by time, then
    // by instance and model so a stacked band keeps its order between two adjacent buckets.
    buckets.sort_by(|a, b| {
        a.start.cmp(&b.start).then_with(|| a.instance.0.cmp(&b.instance.0)).then_with(|| a.model.cmp(&b.model))
    });

    let fully_priced = buckets.iter().all(|b| b.basis == CostBasis::Priced);
    UsageHistory {
        buckets,
        sources,
        resolution,
        since,
        until,
        fully_priced,
        scanned_ms: started.elapsed().as_millis() as u64,
    }
}

#[derive(Default)]
struct Cell {
    usage: Usage,
    requests: u32,
}

impl Cell {
    /// Accumulate. Note this is `+=` and not [`Usage::merge`], which takes the maximum: merge is
    /// right for one message reported repeatedly as it streams, and catastrophically wrong for a
    /// day's worth of separate responses, where it would report the largest single turn as the
    /// day's total.
    fn add(&mut self, u: &Usage) {
        self.usage.input_tokens += u.input_tokens;
        self.usage.output_tokens += u.output_tokens;
        self.usage.cache_read_tokens += u.cache_read_tokens;
        self.usage.cache_write_tokens += u.cache_write_tokens;
        self.usage.thinking_tokens += u.thinking_tokens;
        self.requests += 1;
    }
}

/// The cost of these tokens at these rates, and what the cache saved.
fn priced(u: &Usage, p: &Pricing) -> (f64, f64) {
    let m = 1_000_000.0;
    let cost = (u.input_tokens as f64 * p.input_per_mtok
        + u.output_tokens as f64 * p.output_per_mtok
        + u.cache_read_tokens as f64 * p.cache_read_per_mtok
        + u.cache_write_tokens as f64 * p.cache_write_per_mtok)
        / m;
    // What the cache reads would have cost as ordinary input, minus what they did cost. The
    // difference between a conversation that is expensive and one that only looks it.
    let savings =
        (u.cache_read_tokens as f64 * (p.input_per_mtok - p.cache_read_per_mtok)).max(0.0) / m;
    (cost, savings)
}

/// A price for the model a transcript names.
///
/// Exact first, then longest-prefix. Transcripts carry dated ids — `claude-haiku-4-5-20251001` —
/// where the catalogue has `claude-haiku-4-5`, and a lookup that only matched exactly would price
/// nothing at all on a real machine. Longest wins so `claude-opus-4-5` is not priced by a
/// `claude-opus-4` entry that happens to also be a prefix.
fn lookup<'a>(prices: &'a HashMap<String, Pricing>, model: &str) -> Option<&'a Pricing> {
    if let Some(p) = prices.get(model) {
        return Some(p);
    }
    prices
        .iter()
        .filter(|(id, _)| model.starts_with(id.as_str()))
        .max_by_key(|(id, _)| id.len())
        .map(|(_, p)| p)
}

/// The prices this workspace knows, keyed by model id.
///
/// Built from whatever the model list already holds, which means a provider plugin's models are
/// priced by the same path the built-in ones are — the plugin declared a [`Pricing`] and that is
/// the whole of what it takes to appear on the cost axis.
pub fn price_table(models: &[ModelInfo]) -> HashMap<String, Pricing> {
    models
        .iter()
        .filter_map(|m| m.pricing.clone().map(|p| (m.id.0.clone(), p)))
        .collect()
}

/// Every `.jsonl` under `root`, with its modification time.
///
/// Iterative rather than recursive: these trees are shallow in practice but they are somebody
/// else's, and a symlink loop in a directory we do not own should not be a stack overflow. Depth is
/// capped for the same reason.
fn collect(root: &Path, out: &mut Vec<(PathBuf, i64)>, source: &mut UsageScanSource) {
    const MAX_DEPTH: usize = 8;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                source.status = UsageScanStatus::Partial;
                source.message.get_or_insert_with(|| format!("{}: {e}", dir.display()));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // `file_type` and not `metadata`: it does not follow symlinks, which is what stops a
            // link back up the tree from being walked forever.
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push((path, depth + 1));
            } else if ft.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                let mtime = entry
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_secs() as i64);
                out.push((path, mtime));
            }
        }
    }
}

fn read_file(
    path: &Path,
    kind: VendorKind,
    malformed: &mut u32,
) -> std::io::Result<Vec<Record>> {
    let file = std::fs::File::open(path)?;
    let mut reader = BufReader::new(file.take(FILE_BYTE_LIMIT));
    let gate = kind.gate();
    let mut out = Vec::new();
    let mut codex = CodexState::default();
    let mut line = String::new();

    loop {
        line.clear();
        // Lossy rather than strict: one invalid byte in a tool result is not a reason to abandon a
        // month of history, and `read_line` on a `String` would error out on the whole file.
        let read = read_lossy_line(&mut reader, &mut line)?;
        if read == 0 {
            break;
        }
        // The substring test before the parse. Transcripts are mostly tool output and only a
        // minority of lines carry anything wanted here, so this skips roughly half of a real file
        // for the cost of a scan — worth about an order of magnitude over a month-long span.
        let wanted = gate.iter().any(|g| line.contains(g));
        if !wanted {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim_end()) else {
            *malformed += 1;
            continue;
        };
        let record = match kind {
            VendorKind::Claude => claude_record(&v),
            VendorKind::Codex => codex_record(&v, &mut codex),
        };
        if let Some(r) = record {
            out.push(r);
        }
    }
    Ok(out)
}

/// One line, with invalid UTF-8 replaced rather than fatal.
fn read_lossy_line<R: BufRead>(reader: &mut R, out: &mut String) -> std::io::Result<usize> {
    let mut buf = Vec::new();
    let n = reader.read_until(b'\n', &mut buf)?;
    out.push_str(&String::from_utf8_lossy(&buf));
    Ok(n)
}

/// One `assistant` line of a claude transcript.
fn claude_record(v: &serde_json::Value) -> Option<Record> {
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let message = v.get("message")?;
    let usage = message.get("usage")?;
    let at = parse_time(v.get("timestamp")?.as_str()?)?;
    let model = message.get("model").and_then(|m| m.as_str()).filter(|m| !m.is_empty())?;
    // The CLI's own word for a message no model produced — a local answer to a slash command. It
    // carries a usage block of zeros and would otherwise be a request in the count.
    if model == "<synthetic>" {
        return None;
    }

    let n = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let u = Usage {
        input_tokens: n("input_tokens"),
        output_tokens: n("output_tokens"),
        cache_read_tokens: n("cache_read_input_tokens"),
        cache_write_tokens: n("cache_creation_input_tokens"),
        thinking_tokens: usage
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    };
    if u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens == 0 {
        return None;
    }

    // `requestId` identifies the API call and `message.id` the message it produced. Either alone is
    // enough on a well-formed transcript; both together survive a file where one is missing, which
    // older versions produce.
    let dedupe = match (
        v.get("requestId").and_then(|r| r.as_str()),
        message.get("id").and_then(|i| i.as_str()),
    ) {
        (None, None) => None,
        (req, id) => Some(format!("{}:{}", req.unwrap_or(""), id.unwrap_or(""))),
    };
    Some(Record { at, model: model.to_string(), usage: u, dedupe })
}

/// What a codex rollout needs remembered between its lines.
#[derive(Default)]
struct CodexState {
    model: String,
    /// The last token payload seen, verbatim. Codex re-emits an unchanged `token_count` on some
    /// stream boundaries and summing those would double count.
    last: String,
    saw_meta: bool,
    /// A forked or sub-agent rollout opens with the parent's whole history copied in, every line
    /// re-stamped to the fork instant. Those were already counted from the parent's own file.
    suppressing: bool,
    anchor: i64,
}

/// The copies are written in one burst — observed gaps well under a second — while the child's
/// first genuine turn only lands after a real model round trip. A second of separation splits the
/// two cleanly, which is the threshold `ccusage` settled on for the same reason.
///
/// Seconds, because every timestamp here is. Transcripts are stamped to the millisecond and the
/// parser floors them, so this is the smallest gap that can be told apart at all.
const FORK_COPY_GAP_SECS: i64 = 1;

/// One line of a codex rollout.
///
/// Deltas come from `last_token_usage`, which sums across a session to the `total_token_usage` the
/// session ends with — provided consecutive duplicates are dropped, which is what `last` is for.
fn codex_record(v: &serde_json::Value, state: &mut CodexState) -> Option<Record> {
    let payload = v.get("payload")?;
    match v.get("type").and_then(|t| t.as_str()) {
        Some("session_meta") => {
            // Only the first meta describes this file's own session; a forked rollout repeats its
            // ancestors' right after, and letting those through reassigns everything after them.
            if state.saw_meta {
                return None;
            }
            state.saw_meta = true;
            if is_fork(payload)
                && let Some(at) = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_time)
            {
                state.suppressing = true;
                state.anchor = at;
            }
            return None;
        }
        Some("turn_context") => {
            if let Some(m) = payload.get("model").and_then(|m| m.as_str()) {
                state.model = m.to_string();
            }
            return None;
        }
        _ => {}
    }
    if payload.get("type").and_then(|t| t.as_str()) != Some("token_count") {
        return None;
    }
    let last = payload.pointer("/info/last_token_usage")?;
    let at = v.get("timestamp").and_then(|t| t.as_str()).and_then(parse_time)?;
    // A `token_count` arriving before its `turn_context` has no model yet. Returning here without
    // consuming the duplicate signature is deliberate: the re-emitted copy that follows, once the
    // model is known, must not then be skipped as a repeat and lost entirely.
    if state.model.is_empty() {
        return None;
    }

    let signature = last.to_string();
    if signature == state.last {
        return None;
    }
    state.last = signature;

    if state.suppressing {
        if at - state.anchor < FORK_COPY_GAP_SECS {
            state.anchor = at;
            return None;
        }
        state.suppressing = false;
    }

    let n = |k: &str| last.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let cached = n("cached_input_tokens");
    let written = n("cache_write_input_tokens");
    let output = n("output_tokens");
    let u = Usage {
        // Codex reports `input_tokens` inclusive of the cached part; every other vendor here does
        // not, and a chart that stacked the two would draw this one's prompts twice.
        input_tokens: n("input_tokens").saturating_sub(cached).saturating_sub(written),
        output_tokens: output,
        cache_read_tokens: cached,
        cache_write_tokens: written,
        // Reported inside `output_tokens`, so it is surfaced and never added on top.
        thinking_tokens: n("reasoning_output_tokens").min(output),
    };
    if u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens == 0 {
        return None;
    }
    Some(Record {
        at,
        model: state.model.clone(),
        usage: u,
        // Fork copies are suppressed above, so what survives is unique to this rollout.
        dedupe: None,
    })
}

fn is_fork(payload: &serde_json::Value) -> bool {
    payload.get("forked_from_id").and_then(|v| v.as_str()).is_some()
        || payload
            .pointer("/source/subagent/thread_spawn/parent_thread_id")
            .and_then(|v| v.as_str())
            .is_some()
}

/// The start of the bucket an instant falls in, in the caller's zone.
///
/// `utc_offset` is applied before the division and removed after, which is what makes a day start
/// at local midnight rather than at UTC midnight. Without it a turn at 9pm lands on tomorrow for
/// half the world, and every daily chart is one column out for exactly the people who work late.
fn bucket_start(at: i64, resolution: UsageResolution, utc_offset: i64) -> i64 {
    let span = resolution.seconds();
    let local = at + utc_offset;
    local.div_euclid(span) * span - utc_offset
}

/// An RFC 3339 timestamp as unix seconds. Shares the provider crate's parser so a transcript and a
/// reset time are never read by two different implementations of the same format.
fn parse_time(s: &str) -> Option<i64> {
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
    let mut out = days_from_civil(y, mo, d) * 86_400 + h * 3_600 + mi * 60 + sec;
    let tz = s[19..].trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if !tz.is_empty() && !tz.starts_with(['Z', 'z']) {
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

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(v: &serde_json::Value) -> Option<Record> {
        claude_record(v)
    }

    /// Captured from a real transcript, so the field names are pinned rather than remembered.
    fn real_claude_line() -> serde_json::Value {
        serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-08-21T16:11:47.200Z",
            "requestId": "req_011CeGBpNU1mceR8RB5nhDpH",
            "sessionId": "efa25e98",
            "message": {
                "id": "msg_011CeGBpPntMTQqeZGwCb1sk",
                "model": "claude-opus-5",
                "usage": {
                    "input_tokens": 2,
                    "cache_creation_input_tokens": 14951,
                    "cache_read_input_tokens": 17293,
                    "output_tokens": 420,
                    "output_tokens_details": {"thinking_tokens": 156}
                }
            }
        })
    }

    #[test]
    fn reads_a_real_claude_record() {
        let r = usage(&real_claude_line()).expect("a usage record");
        assert_eq!(r.model, "claude-opus-5");
        assert_eq!(r.usage.input_tokens, 2);
        assert_eq!(r.usage.output_tokens, 420);
        assert_eq!(r.usage.cache_read_tokens, 17_293);
        assert_eq!(r.usage.cache_write_tokens, 14_951);
        assert_eq!(r.usage.thinking_tokens, 156);
        assert_eq!(r.at, parse_time("2026-08-21T16:11:47Z").unwrap());
    }

    /// The whole reason records carry a dedupe key: one message is written once per content block,
    /// each repeating the same complete usage.
    #[test]
    fn repeats_of_one_message_share_a_dedupe_key() {
        let a = usage(&real_claude_line()).unwrap();
        let b = usage(&real_claude_line()).unwrap();
        assert_eq!(a.dedupe, b.dedupe);
        assert!(a.dedupe.is_some());
    }

    /// A local answer to a slash command is not a request and cost nothing.
    #[test]
    fn synthetic_messages_are_not_requests() {
        let mut v = real_claude_line();
        v["message"]["model"] = serde_json::json!("<synthetic>");
        assert!(usage(&v).is_none());
    }

    #[test]
    fn a_line_with_no_tokens_is_not_a_record() {
        let mut v = real_claude_line();
        v["message"]["usage"] = serde_json::json!({"input_tokens": 0, "output_tokens": 0});
        assert!(usage(&v).is_none());
    }

    /// Codex counts cached input inside `input_tokens`; every other vendor here does not.
    #[test]
    fn codex_input_excludes_the_cached_part() {
        let mut state = CodexState::default();
        codex_record(
            &serde_json::json!({"type":"turn_context","timestamp":"2026-08-20T10:00:00Z",
                                "payload":{"model":"gpt-5.6-sol"}}),
            &mut state,
        );
        let r = codex_record(
            &serde_json::json!({
                "type": "event_msg", "timestamp": "2026-08-20T10:00:05Z",
                "payload": {"type":"token_count","info":{"last_token_usage":{
                    "input_tokens": 1000, "cached_input_tokens": 800,
                    "cache_write_input_tokens": 50, "output_tokens": 200,
                    "reasoning_output_tokens": 120}}}
            }),
            &mut state,
        )
        .expect("a usage record");
        assert_eq!(r.model, "gpt-5.6-sol");
        assert_eq!(r.usage.input_tokens, 150, "1000 total, less 800 cached and 50 written");
        assert_eq!(r.usage.cache_read_tokens, 800);
        assert_eq!(r.usage.thinking_tokens, 120);
    }

    /// Codex re-emits an unchanged payload on some stream boundaries.
    #[test]
    fn an_unchanged_codex_payload_is_not_counted_twice() {
        let mut state = CodexState::default();
        codex_record(
            &serde_json::json!({"type":"turn_context","timestamp":"2026-08-20T10:00:00Z",
                                "payload":{"model":"gpt-5.6-sol"}}),
            &mut state,
        );
        let line = serde_json::json!({
            "type": "event_msg", "timestamp": "2026-08-20T10:00:05Z",
            "payload": {"type":"token_count","info":{"last_token_usage":{
                "input_tokens": 10, "output_tokens": 20}}}
        });
        assert!(codex_record(&line, &mut state).is_some());
        assert!(codex_record(&line, &mut state).is_none());
    }

    /// A `token_count` before its `turn_context` must not poison the duplicate signature, or the
    /// re-emitted copy that follows is skipped and those tokens are never counted at all.
    #[test]
    fn a_token_count_before_its_model_does_not_swallow_the_retry() {
        let mut state = CodexState::default();
        let line = serde_json::json!({
            "type": "event_msg", "timestamp": "2026-08-20T10:00:05Z",
            "payload": {"type":"token_count","info":{"last_token_usage":{
                "input_tokens": 10, "output_tokens": 20}}}
        });
        assert!(codex_record(&line, &mut state).is_none(), "no model yet");
        codex_record(
            &serde_json::json!({"type":"turn_context","timestamp":"2026-08-20T10:00:00Z",
                                "payload":{"model":"gpt-5.6-sol"}}),
            &mut state,
        );
        assert!(codex_record(&line, &mut state).is_some(), "the retry must land");
    }

    /// A fork opens with the parent's history re-stamped to one instant. Those tokens were counted
    /// from the parent's own file and counting them again doubles a whole session.
    #[test]
    fn a_forked_rollout_drops_the_copied_history() {
        let mut state = CodexState::default();
        codex_record(
            &serde_json::json!({"type":"session_meta","timestamp":"2026-08-20T10:00:00Z",
                                "payload":{"id":"child","forked_from_id":"parent"}}),
            &mut state,
        );
        codex_record(
            &serde_json::json!({"type":"turn_context","timestamp":"2026-08-20T10:00:00Z",
                                "payload":{"model":"gpt-5.6-sol"}}),
            &mut state,
        );
        // The copies, all stamped at the fork instant.
        for i in 1..4 {
            let line = serde_json::json!({
                "type": "event_msg", "timestamp": "2026-08-20T10:00:00Z",
                "payload": {"type":"token_count","info":{"last_token_usage":{
                    "input_tokens": 10 * i, "output_tokens": 20}}}
            });
            assert!(codex_record(&line, &mut state).is_none(), "copy {i} must be dropped");
        }
        // A real turn, seconds later.
        let real = serde_json::json!({
            "type": "event_msg", "timestamp": "2026-08-20T10:00:30Z",
            "payload": {"type":"token_count","info":{"last_token_usage":{
                "input_tokens": 99, "output_tokens": 20}}}
        });
        assert!(codex_record(&real, &mut state).is_some());
    }

    /// The substring gate has to let through every line the parser needs, not only the one the
    /// records come from.
    ///
    /// Driven through `read_file` rather than the parser, because the gate is the part being
    /// tested: gating codex on `token_count` alone drops the `turn_context` that carries the model,
    /// and a record with no model is discarded — so a whole rollout silently counts as nothing,
    /// with no error anywhere and a plausible-looking empty chart.
    #[test]
    fn the_line_gate_lets_through_everything_the_parser_needs() {
        let dir = std::env::temp_dir().join(format!("neosh-usage-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("rollout.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"session_meta","timestamp":"2026-08-20T10:00:00Z","payload":{"id":"s"}}"#,
                "
",
                r#"{"type":"turn_context","timestamp":"2026-08-20T10:00:01Z","payload":{"model":"gpt-5.6-sol"}}"#,
                "
",
                r#"{"type":"response_item","timestamp":"2026-08-20T10:00:02Z","payload":{"type":"message","text":"nothing to count"}}"#,
                "
",
                r#"{"type":"event_msg","timestamp":"2026-08-20T10:00:05Z","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":30,"output_tokens":9}}}}"#,
                "
",
            ),
        )
        .expect("write");

        let mut malformed = 0;
        let records = read_file(&path, VendorKind::Codex, &mut malformed).expect("read");
        assert_eq!(records.len(), 1, "one usage record");
        assert_eq!(records[0].model, "gpt-5.6-sol", "and it knows which model produced it");
        assert_eq!(records[0].usage.output_tokens, 9);
        assert_eq!(malformed, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A line that looks like it carries usage and will not parse is counted, not ignored.
    ///
    /// The count is what turns "your chart is empty" into "we could not read your transcripts",
    /// which are different problems with different fixes.
    #[test]
    fn an_unparseable_line_that_claimed_usage_is_counted() {
        let dir = std::env::temp_dir().join(format!("neosh-usage-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let path = dir.join("broken.jsonl");
        std::fs::write(&path, "{\"type\":\"assistant\",\"usage\":\n").expect("write");
        let mut malformed = 0;
        let records = read_file(&path, VendorKind::Claude, &mut malformed).expect("read");
        assert!(records.is_empty());
        assert_eq!(malformed, 1, "counted, so the panel can say the scan was partial");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dated transcript id priced by the undated catalogue entry, which is the only way anything
    /// gets priced on a real machine.
    #[test]
    fn prices_match_on_the_longest_prefix() {
        let mut prices = HashMap::new();
        prices.insert("claude-opus-4".to_string(), Pricing {
            input_per_mtok: 1.0, output_per_mtok: 1.0, ..Default::default()
        });
        prices.insert("claude-opus-4-5".to_string(), Pricing {
            input_per_mtok: 2.0, output_per_mtok: 2.0, ..Default::default()
        });
        let p = lookup(&prices, "claude-opus-4-5-20260101").expect("a price");
        assert_eq!(p.input_per_mtok, 2.0, "the longer prefix wins");
        assert!(lookup(&prices, "gpt-5.6-sol").is_none());
    }

    /// Summing a day, not taking its maximum. `Usage::merge` is right for one streaming message and
    /// would report the largest single turn as the whole day's total.
    #[test]
    fn a_cell_sums_rather_than_maxes() {
        let mut cell = Cell::default();
        cell.add(&Usage { output_tokens: 100, ..Default::default() });
        cell.add(&Usage { output_tokens: 50, ..Default::default() });
        assert_eq!(cell.usage.output_tokens, 150);
        assert_eq!(cell.requests, 2);
    }

    /// A day starts at local midnight. At UTC it would be one column out for anybody working late.
    #[test]
    fn days_are_bucketed_in_the_callers_zone() {
        // 2026-08-21T23:30:00Z, which is already the 22nd at +02:00.
        let at = parse_time("2026-08-21T23:30:00Z").unwrap();
        let utc = bucket_start(at, UsageResolution::Day, 0);
        let plus2 = bucket_start(at, UsageResolution::Day, 2 * 3_600);
        assert_eq!(utc, parse_time("2026-08-21T00:00:00Z").unwrap());
        assert_eq!(plus2, parse_time("2026-08-21T22:00:00Z").unwrap(), "local midnight of the 22nd");
    }

    /// A negative offset has to round the same way a positive one does, which integer division
    /// towards zero does not: `-1 / 86400` is `0`, so a turn in the small hours west of UTC would
    /// bucket to the *next* day and the chart would gain a column that had not happened yet.
    #[test]
    fn bucketing_is_euclidean_so_it_works_west_of_utc() {
        // 03:00 UTC is 19:00 the previous evening in Los Angeles.
        let at = parse_time("2026-08-21T03:00:00Z").unwrap();
        let minus8 = bucket_start(at, UsageResolution::Day, -8 * 3_600);
        assert_eq!(minus8, parse_time("2026-08-20T08:00:00Z").unwrap(), "local midnight of the 20th");
        assert_eq!(at - minus8, 19 * 3_600, "nineteen hours into the 20th, locally");
    }

    /// One instant is a different calendar day in two zones, and each has to bucket to *its own*
    /// local midnight. The two bucket starts are seventeen hours apart, not a whole day: they are
    /// the midnights of consecutive days minus the seventeen hours between the zones.
    #[test]
    fn two_zones_disagree_about_which_day_an_instant_is_on() {
        let at = parse_time("2026-08-21T03:00:00Z").unwrap();
        let la = bucket_start(at, UsageResolution::Day, -8 * 3_600);
        let tokyo = bucket_start(at, UsageResolution::Day, 9 * 3_600);
        assert_eq!(la, parse_time("2026-08-20T08:00:00Z").unwrap(), "midnight of the 20th, LA");
        assert_eq!(tokyo, parse_time("2026-08-20T15:00:00Z").unwrap(), "midnight of the 21st, Tokyo");
        assert_ne!(la, tokyo, "the same turn is not in the same cell for both");
    }

    #[test]
    fn hours_bucket_to_the_hour() {
        let at = parse_time("2026-08-21T16:47:13Z").unwrap();
        assert_eq!(
            bucket_start(at, UsageResolution::Hour, 0),
            parse_time("2026-08-21T16:00:00Z").unwrap()
        );
    }
}
