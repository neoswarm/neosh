//! Version-control wire types.
//!
//! A plugin cannot run `git` itself — the script runtime has no process access, by design. So the
//! host owns the porcelain and hands back these types, which means a sidebar, a branch picker and a
//! commit UI are all plugins over one vocabulary rather than three shell-outs with three parsers.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Where a repository is and what state its HEAD is in.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct RepoInfo {
    /// Absolute path to the working tree root.
    pub root: String,
    /// `None` on a detached HEAD, which is why `head` is reported separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub detached: bool,
    /// Short hash of HEAD. `None` in a repository with no commits yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default)]
    pub ahead: u32,
    #[serde(default)]
    pub behind: u32,
}

/// What happened to one path, as reported by `git status --porcelain=v2`.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum FileState {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Ignored,
    /// Both sides changed it. Committing without resolving is an error, so this is called out
    /// separately rather than folded into `Modified`.
    Conflicted,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct FileChange {
    pub path: String,
    /// Set only for renames and copies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// State of the staged copy, if the path is staged at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged: Option<FileState>,
    /// State of the working-tree copy, if it differs from the index.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unstaged: Option<FileState>,
}

impl FileChange {
    pub fn is_conflicted(&self) -> bool {
        self.staged == Some(FileState::Conflicted) || self.unstaged == Some(FileState::Conflicted)
    }
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[ts(export)]
pub struct RepoStatus {
    pub repo: RepoInfo,
    #[serde(default)]
    pub changes: Vec<FileChange>,
}

impl RepoStatus {
    pub fn is_dirty(&self) -> bool {
        !self.changes.is_empty()
    }

    pub fn staged_count(&self) -> usize {
        self.changes.iter().filter(|c| c.staged.is_some()).count()
    }
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct BranchInfo {
    pub name: String,
    #[serde(default)]
    pub is_head: bool,
    #[serde(default)]
    pub is_remote: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default)]
    pub ahead: u32,
    #[serde(default)]
    pub behind: u32,
    /// Subject of the tip commit, for a picker that wants a second line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// Committer date of the tip, seconds since the epoch. Sorting a branch list by anything else
    /// puts the branch you were just on somewhere in the middle.
    #[serde(default)]
    #[ts(type = "number")]
    pub committed_at: i64,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct WorktreeInfo {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The original checkout, as opposed to one `git worktree add` created.
    #[serde(default)]
    pub is_main: bool,
    #[serde(default)]
    pub locked: bool,
    /// True for the worktree this neosh is running in.
    #[serde(default)]
    pub is_current: bool,
}

#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct CommitInfo {
    pub hash: String,
    pub short: String,
    pub subject: String,
    #[serde(default)]
    pub body: String,
    pub author: String,
    #[serde(default)]
    #[ts(type = "number")]
    pub committed_at: i64,
}

/// What to run `git diff` against.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(export)]
pub enum DiffTarget {
    /// Working tree vs index.
    #[default]
    Unstaged,
    /// Index vs HEAD. What a commit would contain.
    Staged,
    /// `base...head`, the merge-base diff a pull request shows.
    Range { base: String, head: String },
}

// ---------------------------------------------------------------------------
// The forge
// ---------------------------------------------------------------------------

/// Where a pull request has got to.
///
/// Four states because that is what a person means by "what happened to it": one is a review you
/// are waiting for, one is a review you have not asked for yet, one is done, and one is not going
/// to happen. Draft is its own state rather than a flag on `Open` because on a row it is the whole
/// difference between "somebody should look at this" and "I am still writing it".
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum PullState {
    Open,
    Draft,
    Merged,
    Closed,
}

/// What the forge's checks say, folded to the one thing a row has room for.
///
/// Folded here rather than on the wire as a list, because the question a project row answers is
/// "is anything wrong" and the answer is one glyph. The counts travel beside it for whoever wants
/// to say `✗2`; the full list of check names is a panel's job and a different call.
///
/// `Pending` and `None` are deliberately different. Nothing has run yet is a state that resolves on
/// its own in a few minutes; there are no checks configured at all is permanent, and drawing a
/// spinner over it for ever is the failure this distinction avoids.
#[derive(TS, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum ChecksState {
    /// This repository runs no checks on this branch.
    None,
    /// At least one is still going, and nothing has failed yet.
    Pending,
    Passing,
    Failing,
}

/// One pull request, as much of it as a row can use.
///
/// **Keyed by `branch`**, which is what makes one call answer for every worktree at once: a
/// repository's pull requests come back together and each checkout finds its own by the branch it
/// has out. Asking per branch would be one network round trip per row in the panel.
#[derive(TS, Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[ts(export)]
pub struct PullRequest {
    #[ts(type = "number")]
    pub number: u64,
    pub title: String,
    /// For opening it. The forge's own URL, so nothing here has to know how to build one.
    pub url: String,
    /// The head branch. What a checkout matches itself against.
    pub branch: String,
    pub state: PullState,
    pub checks: ChecksState,
    /// How many checks have failed, for a row that wants to say `✗2` rather than `✗`.
    #[serde(default)]
    pub checks_failed: u32,
    #[serde(default)]
    pub checks_total: u32,
}
