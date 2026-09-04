//! What the *forge* knows about a repository, which git does not.
//!
//! A branch is a local fact and a pull request is not: whether one exists, whether anybody has
//! looked at it, and whether its checks went green are all answers held on somebody else's server.
//! Git has no opinion about any of it, so a panel that draws branches and stops there is a panel
//! that can tell you a worktree is three commits ahead and not that those three commits have been
//! sitting in review since Tuesday.
//!
//! # Why `gh`
//!
//! The same trade this crate makes for git, one layer up. `gh` is the CLI the user has already
//! signed into, and it already handles the parts that are genuinely hard: OAuth and token refresh,
//! `GH_ENTERPRISE_TOKEN` and self-hosted hosts, SSO authorisation on organisation repositories,
//! the credential helper, and rate-limit backoff. A neosh that spoke to the API directly would be
//! re-implementing all of that in order to arrive at the same answer, and would then have a second
//! place for a token to live.
//!
//! It also means the answer is **the same answer the user gets in their own terminal**, which is
//! the property that makes a disagreement debuggable: `gh pr list` beside the sidebar, and if they
//! differ the bug is here.
//!
//! # One call for the whole panel
//!
//! Pull requests come back per *repository*, keyed by head branch, so a repository with six
//! worktrees is one request and six lookups rather than six requests. That is the difference
//! between a sidebar that costs a round trip every few minutes and one that costs six, and it is
//! why [`PullRequest::branch`] is on the wire at all.

use std::path::Path;
use std::process::Stdio;

use neosh_proto::{ChecksState, PullRequest, PullState};
use tokio::process::Command;

use crate::VcsError;

/// How many pull requests to ask for.
///
/// Generous, because the join is by branch and a branch whose pull request fell off the end of the
/// list reads as "no pull request" — which is the one wrong answer available here. A repository with
/// more than this many is one where the oldest are closed and nobody has a worktree on them.
const LIMIT: usize = 200;

/// The fields, named once. `statusCheckRollup` is the expensive one and the reason this is not
/// asked more often than it is.
const FIELDS: &str = "number,state,isDraft,headRefName,title,url,statusCheckRollup";

/// Every pull request for the repository containing `dir`, open and closed.
///
/// Fails rather than answering empty when it cannot know. An empty list is a *claim* — "this
/// repository has no pull requests" — and three of the four ways this call goes wrong would be
/// making that claim falsely, on a panel whose whole job is telling you what is waiting.
pub async fn pulls(dir: &Path) -> Result<Vec<PullRequest>, VcsError> {
    let out = run_gh(dir, &["pr", "list", "--state", "all", "--limit", &LIMIT.to_string(),
                            "--json", FIELDS]).await?;
    parse_pulls(&out)
}

/// Run `gh` in `dir`, with the same scrub `git` gets and one more.
///
/// `GH_PROMPT_DISABLED` is the counterpart to `GIT_TERMINAL_PROMPT=0`: `gh` will otherwise offer to
/// open a browser and wait, which under a TUI is a hang with no visible cause. `NO_COLOR` because
/// the answer is parsed, and a stray escape sequence in a title is a JSON error rather than a
/// colour.
async fn run_gh(dir: &Path, args: &[&str]) -> Result<String, VcsError> {
    let mut cmd = Command::new("gh");
    cmd.current_dir(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0");

    let out = match cmd.output().await {
        Ok(o) => o,
        // The common case on a machine that has never installed it, and the one message that has to
        // say what to do rather than what happened.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(VcsError::ForgeUnavailable {
                message: "the GitHub CLI is not installed — `brew install gh`, then `gh auth login`"
                    .into(),
            });
        }
        Err(e) => return Err(VcsError::Io(e)),
    };
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    let said = stderr.trim();
    // Three refusals that are not failures, told apart because only one of them is worth a word on
    // screen and none of them is worth a toast every few minutes. A repository with no remote is
    // the ordinary state of a scratch checkout; being signed out is a thing to fix once.
    let message = if said.contains("no git remotes") || said.contains("none of the git remotes") {
        "this repository has no remote on a forge".to_string()
    } else if said.contains("gh auth login") || said.contains("authentication") {
        "not signed in to the forge — run `gh auth login`".to_string()
    } else {
        said.lines().next().unwrap_or("gh failed").to_string()
    };
    Err(VcsError::ForgeUnavailable { message })
}

/// `gh`'s JSON into the wire types, every field checked rather than trusted.
///
/// A separate function so it can be tested against captured output: `gh`'s shape is somebody else's
/// and changes with their releases, which is exactly the kind of thing that should fail in a test
/// rather than as an empty sidebar.
fn parse_pulls(json: &str) -> Result<Vec<PullRequest>, VcsError> {
    let raw: serde_json::Value = serde_json::from_str(json).map_err(|e| VcsError::Command {
        command: "gh pr list".into(),
        message: format!("could not read what gh answered: {e}"),
    })?;
    let Some(items) = raw.as_array() else {
        return Err(VcsError::Command {
            command: "gh pr list".into(),
            message: "gh answered something that is not a list of pull requests".into(),
        });
    };

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(number) = item.get("number").and_then(serde_json::Value::as_u64) else { continue };
        let branch = item.get("headRefName").and_then(|v| v.as_str()).unwrap_or("");
        if branch.is_empty() {
            // The join key. A pull request nothing can be matched against is one no row would ever
            // draw, so it is dropped here rather than carried to every caller to be skipped again.
            continue;
        }
        let draft = item.get("isDraft").and_then(serde_json::Value::as_bool).unwrap_or(false);
        // Draft outranks open, because on a row it is the whole difference between "somebody should
        // look at this" and "I am still writing it". `gh` reports a draft as `OPEN` with a flag.
        let state = match item.get("state").and_then(|v| v.as_str()).unwrap_or("") {
            "MERGED" => PullState::Merged,
            "CLOSED" => PullState::Closed,
            _ if draft => PullState::Draft,
            _ => PullState::Open,
        };
        let (checks, checks_failed, checks_total) = rollup(item.get("statusCheckRollup"));
        out.push(PullRequest {
            number,
            title: item.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            url: item.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            branch: branch.to_string(),
            state,
            checks,
            checks_failed,
            checks_total,
        });
    }
    Ok(out)
}

/// Every check on one pull request, folded to the one thing a row has room for.
///
/// **A check that has not finished is not a check that passed.** The rollup is a list of runs in
/// every state at once, and the order the states are tested in *is* the answer: anything failing
/// makes the whole thing failing however many others went green, anything still running makes it
/// pending, and only a set where everything has finished and nothing failed is passing. Testing
/// "any success" first — which is the easy mistake, because it is the field you are looking for —
/// draws a tick over a run that is thirty seconds from going red.
///
/// An empty rollup is [`ChecksState::None`] rather than passing, because a repository that runs no
/// checks has not told you anything and a green tick would say it had.
///
/// Both the CheckRun and StatusContext shapes, because a repository with a legacy status API
/// integration — most of them, somewhere — gets `state` where a GitHub Action gets `conclusion`.
fn rollup(value: Option<&serde_json::Value>) -> (ChecksState, u32, u32) {
    let Some(runs) = value.and_then(|v| v.as_array()) else { return (ChecksState::None, 0, 0) };
    if runs.is_empty() {
        return (ChecksState::None, 0, 0);
    }
    let (mut failed, mut pending) = (0u32, 0u32);
    for run in runs {
        // `conclusion` on a CheckRun, `state` on a legacy StatusContext. A run that is still going
        // has an empty conclusion and a `status` of `IN_PROGRESS` or `QUEUED`.
        let verdict = run
            .get("conclusion")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| run.get("state").and_then(|v| v.as_str()))
            .unwrap_or("");
        match verdict {
            "FAILURE" | "TIMED_OUT" | "STARTUP_FAILURE" | "ERROR" => failed += 1,
            // Skipped, neutral and cancelled are not failures and are not news. A workflow that
            // skipped every job on a docs-only change must not paint the row red.
            "SUCCESS" | "SKIPPED" | "NEUTRAL" | "CANCELLED" | "ACTION_REQUIRED" => {}
            // Everything else — `PENDING`, `IN_PROGRESS`, `QUEUED`, `EXPECTED`, or an empty
            // conclusion on a run that has not finished — is still out.
            _ => pending += 1,
        }
    }
    let total = runs.len() as u32;
    let state = if failed > 0 {
        ChecksState::Failing
    } else if pending > 0 {
        ChecksState::Pending
    } else {
        ChecksState::Passing
    };
    (state, failed, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_is_its_own_state_rather_than_a_flag_on_open() {
        let pulls = parse_pulls(
            r#"[{"number":1,"state":"OPEN","isDraft":true,"headRefName":"wip","title":"t",
                 "url":"u","statusCheckRollup":[]},
                {"number":2,"state":"OPEN","isDraft":false,"headRefName":"ready","title":"t",
                 "url":"u","statusCheckRollup":[]},
                {"number":3,"state":"MERGED","isDraft":false,"headRefName":"done","title":"t",
                 "url":"u","statusCheckRollup":[]}]"#,
        )
        .expect("parses");
        assert_eq!(pulls.len(), 3);
        assert_eq!(pulls[0].state, PullState::Draft, "a draft reads as OPEN with a flag");
        assert_eq!(pulls[1].state, PullState::Open);
        assert_eq!(pulls[2].state, PullState::Merged);
    }

    #[test]
    fn a_check_that_has_not_finished_is_not_a_check_that_passed() {
        // The mistake this pins: two green and one still running is *pending*. Testing for a
        // success first draws a tick over a run thirty seconds from going red.
        let mixed = r#"[{"number":1,"state":"OPEN","isDraft":false,"headRefName":"b","title":"t",
            "url":"u","statusCheckRollup":[
              {"conclusion":"SUCCESS"},{"conclusion":"SUCCESS"},
              {"conclusion":"","status":"IN_PROGRESS"}]}]"#;
        let p = &parse_pulls(mixed).expect("parses")[0];
        assert_eq!(p.checks, ChecksState::Pending);

        // And anything failing outranks everything else, however many went green.
        let red = r#"[{"number":1,"state":"OPEN","isDraft":false,"headRefName":"b","title":"t",
            "url":"u","statusCheckRollup":[
              {"conclusion":"SUCCESS"},{"conclusion":"FAILURE"},{"conclusion":"","status":"QUEUED"}]}]"#;
        let p = &parse_pulls(red).expect("parses")[0];
        assert_eq!(p.checks, ChecksState::Failing);
        assert_eq!(p.checks_failed, 1);
        assert_eq!(p.checks_total, 3);
    }

    #[test]
    fn no_checks_configured_is_not_a_green_tick() {
        let none = r#"[{"number":1,"state":"OPEN","isDraft":false,"headRefName":"b","title":"t",
            "url":"u","statusCheckRollup":[]}]"#;
        assert_eq!(parse_pulls(none).expect("parses")[0].checks, ChecksState::None);
        // Missing entirely, which is what an older `gh` answers, is the same answer.
        let absent = r#"[{"number":1,"state":"OPEN","isDraft":false,"headRefName":"b","title":"t",
            "url":"u"}]"#;
        assert_eq!(parse_pulls(absent).expect("parses")[0].checks, ChecksState::None);
    }

    #[test]
    fn skipped_and_cancelled_runs_do_not_paint_the_row_red() {
        // A docs-only change skips every job. That is a pull request with nothing wrong with it.
        let skipped = r#"[{"number":1,"state":"OPEN","isDraft":false,"headRefName":"b","title":"t",
            "url":"u","statusCheckRollup":[{"conclusion":"SKIPPED"},{"conclusion":"SUCCESS"}]}]"#;
        let p = &parse_pulls(skipped).expect("parses")[0];
        assert_eq!(p.checks, ChecksState::Passing);
        assert_eq!(p.checks_failed, 0);
    }

    #[test]
    fn the_legacy_status_shape_is_read_too() {
        // A `StatusContext` from the older API carries `state` where a CheckRun carries
        // `conclusion`. Most repositories have one of these somewhere.
        let legacy = r#"[{"number":1,"state":"OPEN","isDraft":false,"headRefName":"b","title":"t",
            "url":"u","statusCheckRollup":[{"state":"FAILURE"},{"state":"SUCCESS"}]}]"#;
        let p = &parse_pulls(legacy).expect("parses")[0];
        assert_eq!(p.checks, ChecksState::Failing);
        assert_eq!(p.checks_failed, 1);
    }

    #[test]
    fn a_pull_request_with_no_branch_is_dropped_rather_than_carried() {
        // The branch is the join key; one without it is a row nothing could ever draw.
        let pulls = parse_pulls(
            r#"[{"number":1,"state":"OPEN","isDraft":false,"headRefName":"","title":"t","url":"u"},
                {"number":2,"state":"OPEN","isDraft":false,"headRefName":"b","title":"t","url":"u"}]"#,
        )
        .expect("parses");
        assert_eq!(pulls.len(), 1);
        assert_eq!(pulls[0].number, 2);
    }

    #[test]
    fn something_that_is_not_a_list_is_an_error_rather_than_an_empty_panel() {
        // An empty list is a claim — "no pull requests" — and gh answering something unexpected
        // must not be able to make it.
        assert!(parse_pulls("{}").is_err());
        assert!(parse_pulls("not json").is_err());
    }
}
