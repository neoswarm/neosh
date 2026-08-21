//! Git, as a set of async calls returning [`neosh_proto`] types.
//!
//! Implemented by shelling out to `git` rather than by linking a library. That is a deliberate
//! trade: `git` is already installed wherever this runs, it is the only implementation that agrees
//! with itself about worktrees, hooks, config and credential helpers, and a wrong answer from it is
//! the same wrong answer the user gets in their own terminal.
//!
//! Every call is `async` and every call is *fallible* — a directory that is not a repository is an
//! ordinary answer here, not an error to be unwrapped.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use neosh_proto::{
    BranchInfo, CommitInfo, DiffTarget, RepoInfo, RepoStatus, WorktreeInfo,
};
use tokio::process::Command;

mod parse;

pub use parse::{parse_porcelain_v2, parse_worktree_list};

#[derive(Debug, thiserror::Error)]
pub enum VcsError {
    #[error("not a git repository: {0}")]
    NotARepository(String),
    #[error("git is not installed or not on PATH")]
    GitMissing,
    #[error("git {command} failed: {message}")]
    Command { command: String, message: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Output of one `git` invocation that exited zero.
struct Output {
    stdout: String,
}

/// A working tree.
///
/// Cheap to clone and safe to hold across awaits; it is just a path plus the knowledge that a
/// repository was there when we looked.
#[derive(Debug, Clone)]
pub struct Git {
    root: PathBuf,
    /// The directory neosh was started in, which is not necessarily the root — it decides which
    /// worktree is "current".
    cwd: PathBuf,
}

impl Git {
    /// Find the repository containing `cwd`, or `None` if there is not one.
    pub async fn discover(cwd: impl AsRef<Path>) -> Option<Self> {
        let cwd = cwd.as_ref().to_path_buf();
        let out = run(&cwd, ["rev-parse", "--show-toplevel"]).await.ok()?;
        let root = out.stdout.trim();
        if root.is_empty() {
            return None;
        }
        Some(Self { root: PathBuf::from(root), cwd })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// What to call this checkout, and what it is a checkout *of*.
    ///
    /// Returns `(branch, main worktree)`. Both are needed to name a directory usefully, and
    /// neither is [`Self::root`]: in a linked worktree `--show-toplevel` is the worktree itself,
    /// so the repository's own name has to come from `--git-common-dir`, whose parent is the
    /// original checkout.
    ///
    /// One `rev-parse` for both, and deliberately not [`Self::info`] — that runs a full
    /// `git status`, which is the wrong price to pay for a label in a list.
    pub async fn identity(&self) -> (Option<String>, Option<PathBuf>) {
        let Ok(out) = self
            .git(["rev-parse", "--path-format=absolute", "--git-common-dir", "--abbrev-ref", "HEAD"])
            .await
        else {
            return (None, None);
        };
        let mut lines = out.stdout.lines().map(str::trim).filter(|l| !l.is_empty());
        // `--git-common-dir` is the `.git` of the original checkout; its parent is that checkout.
        // A bare repository has no parent worth naming, which `None` says.
        let main = lines
            .next()
            .map(PathBuf::from)
            .and_then(|g| g.parent().map(Path::to_path_buf))
            .filter(|p| p.is_dir());
        // `HEAD` on a detached head, which is not a branch name and should not be shown as one.
        let branch = lines.next().filter(|b| *b != "HEAD").map(str::to_string);
        (branch, main)
    }

    async fn git<I, S>(&self, args: I) -> Result<Output, VcsError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        run(&self.root, args).await
    }

    // ---- reading --------------------------------------------------------

    /// The `origin` remote's URL, if there is one.
    ///
    /// The only thing about a checkout that is the same on every machine that has it, which is what
    /// makes it the basis for saying two computers have the same project. `None` for a repository
    /// with no remote — a perfectly ordinary state, and one where there is nothing to compare.
    pub async fn origin_url(&self) -> Option<String> {
        let out = self.git(["remote", "get-url", "origin"]).await.ok()?;
        let url = out.stdout.trim();
        (!url.is_empty()).then(|| url.to_string())
    }

    /// Branch, upstream and ahead/behind.
    pub async fn info(&self) -> Result<RepoInfo, VcsError> {
        Ok(self.status().await?.repo)
    }

    /// Full working-tree status.
    ///
    /// Uses `--porcelain=v2 --branch`, which is the only status format git promises not to change
    /// and the only one that reports ahead/behind without a second call.
    pub async fn status(&self) -> Result<RepoStatus, VcsError> {
        let out = self
            .git(["status", "--porcelain=v2", "--branch", "--untracked-files=normal", "-z"])
            .await?;
        let mut status = parse_porcelain_v2(&out.stdout);
        status.repo.root = self.root.display().to_string();
        Ok(status)
    }

    /// Local branches, most recently committed first.
    ///
    /// `include_remote` adds remote-tracking branches, which a "check out a teammate's branch"
    /// flow needs and a "switch to my other feature" flow does not.
    pub async fn branches(&self, include_remote: bool) -> Result<Vec<BranchInfo>, VcsError> {
        // committerdate, not authordate: a branch you rebased five minutes ago should sort as five
        // minutes old, which is what you expect when looking for the one you were just on.
        const FORMAT: &str = "%(refname:short)%09%(HEAD)%09%(upstream:short)%09%(upstream:track)%09%(committerdate:unix)%09%(contents:subject)";
        let mut args = vec![
            "for-each-ref".to_string(),
            format!("--format={FORMAT}"),
            "--sort=-committerdate".to_string(),
            "refs/heads".to_string(),
        ];
        if include_remote {
            args.push("refs/remotes".to_string());
        }
        let out = self.git(args).await?;
        Ok(out.stdout.lines().filter_map(parse::branch_line).collect())
    }

    pub async fn worktrees(&self) -> Result<Vec<WorktreeInfo>, VcsError> {
        let out = self.git(["worktree", "list", "--porcelain"]).await?;
        let mut trees = parse_worktree_list(&out.stdout);
        // `git worktree list` says nothing about which tree the caller is standing in. Resolve both
        // ends before comparing, so a symlinked or relative path still matches.
        let here = std::fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone());
        for t in &mut trees {
            let p = PathBuf::from(&t.path);
            let p = std::fs::canonicalize(&p).unwrap_or(p);
            t.is_current = here.starts_with(&p);
        }
        Ok(trees)
    }

    pub async fn log(&self, limit: u32) -> Result<Vec<CommitInfo>, VcsError> {
        // NUL between fields and RS between records: a commit subject may contain a tab, and a body
        // certainly contains newlines, so any printable separator eventually corrupts a record.
        let out = self
            .git([
                "log".to_string(),
                format!("--max-count={limit}"),
                "--format=%H%x00%h%x00%an%x00%ct%x00%s%x00%b%x1e".to_string(),
            ])
            .await?;
        Ok(parse::commits(&out.stdout))
    }

    pub async fn diff(&self, target: &DiffTarget) -> Result<String, VcsError> {
        Ok(self.git(diff_args(target, false)).await?.stdout)
    }

    /// `--stat` form of the same diff, for a prompt that must not carry 40 000 lines of patch.
    pub async fn diff_stat(&self, target: &DiffTarget) -> Result<String, VcsError> {
        Ok(self.git(diff_args(target, true)).await?.stdout)
    }

    /// The branch this one would merge into, from `origin/HEAD`, falling back to whichever of
    /// `main`/`master` exists.
    pub async fn default_branch(&self) -> Option<String> {
        if let Ok(out) = self.git(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await {
            let s = out.stdout.trim();
            if let Some(name) = s.strip_prefix("origin/")
                && !name.is_empty()
            {
                return Some(name.to_string());
            }
        }
        for candidate in ["main", "master"] {
            if self
                .git(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{candidate}")])
                .await
                .is_ok()
            {
                return Some(candidate.to_string());
            }
        }
        None
    }

    // ---- writing --------------------------------------------------------

    /// Create a branch and switch to it. `from` defaults to current HEAD.
    pub async fn create_branch(&self, name: &str, from: Option<&str>) -> Result<(), VcsError> {
        let mut args = vec!["switch".to_string(), "--create".to_string(), name.to_string()];
        if let Some(from) = from {
            args.push(from.to_string());
        }
        self.git(args).await.map(|_| ())
    }

    pub async fn checkout(&self, rev: &str) -> Result<(), VcsError> {
        self.git(["switch", rev]).await.map(|_| ())
    }

    pub async fn stage(&self, paths: &[String]) -> Result<(), VcsError> {
        let mut args = vec!["add".to_string(), "--".to_string()];
        if paths.is_empty() {
            args.push(".".to_string());
        } else {
            args.extend(paths.iter().cloned());
        }
        self.git(args).await.map(|_| ())
    }

    pub async fn unstage(&self, paths: &[String]) -> Result<(), VcsError> {
        let mut args = vec!["restore".to_string(), "--staged".to_string(), "--".to_string()];
        if paths.is_empty() {
            args.push(".".to_string());
        } else {
            args.extend(paths.iter().cloned());
        }
        self.git(args).await.map(|_| ())
    }

    /// Commit what is staged, returning the new commit.
    ///
    /// The message is one `--message` argument, never a shell string, so a subject containing
    /// quotes, newlines or `$(…)` is data rather than code.
    pub async fn commit(&self, message: &str) -> Result<CommitInfo, VcsError> {
        self.git(["commit", "--message", message]).await?;
        self.log(1).await?.pop().ok_or_else(|| VcsError::Command {
            command: "commit".into(),
            message: "commit succeeded but HEAD is unreadable".into(),
        })
    }

    pub async fn add_worktree(
        &self,
        path: &Path,
        branch: &str,
        create: bool,
    ) -> Result<(), VcsError> {
        let mut args = vec!["worktree".to_string(), "add".to_string()];
        if create {
            args.push("-b".to_string());
            args.push(branch.to_string());
            args.push(path.display().to_string());
        } else {
            args.push(path.display().to_string());
            args.push(branch.to_string());
        }
        self.git(args).await.map(|_| ())
    }

    pub async fn remove_worktree(&self, path: &Path, force: bool) -> Result<(), VcsError> {
        let mut args = vec!["worktree".to_string(), "remove".to_string()];
        if force {
            args.push("--force".to_string());
        }
        args.push(path.display().to_string());
        self.git(args).await.map(|_| ())
    }

    /// Everything a status line or a prompt builder needs, gathered concurrently.
    ///
    /// One call rather than five, so a sidebar refresh does not fan out five subprocesses.
    pub async fn snapshot(&self, log_limit: u32) -> Snapshot {
        let (status, branches, worktrees, recent, default_branch) = tokio::join!(
            self.status(),
            self.branches(false),
            self.worktrees(),
            self.log(log_limit),
            self.default_branch(),
        );
        Snapshot {
            status: status.unwrap_or_default(),
            branches: branches.unwrap_or_default(),
            worktrees: worktrees.unwrap_or_default(),
            recent: recent.unwrap_or_default(),
            default_branch,
        }
    }
}

fn diff_args(target: &DiffTarget, stat: bool) -> Vec<String> {
    let mut args: Vec<String> = match target {
        DiffTarget::Unstaged => vec!["diff".into()],
        DiffTarget::Staged => vec!["diff".into(), "--cached".into()],
        DiffTarget::Range { base, head } => vec!["diff".into(), format!("{base}...{head}")],
    };
    if stat {
        args.push("--stat".into());
    }
    args
}

/// Run one `git` command in `dir`.
///
/// The environment is scrubbed of everything that makes git interactive: a credential prompt or a
/// pager started underneath a TUI is a hang with no visible cause.
async fn run<I, S>(dir: &Path, args: I) -> Result<Output, VcsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<std::ffi::OsString> =
        args.into_iter().map(|a| a.as_ref().to_os_string()).collect();
    let display =
        args.iter().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");

    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
    cmd.env_remove("GIT_ASKPASS");
    cmd.env_remove("SSH_ASKPASS");

    let out = match cmd.output().await {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(VcsError::GitMissing),
        Err(e) => return Err(VcsError::Io(e)),
    };

    if out.status.success() {
        return Ok(Output { stdout: String::from_utf8_lossy(&out.stdout).into_owned() });
    }

    let stderr = String::from_utf8_lossy(&out.stderr);
    let message = stderr.trim();
    if message.contains("not a git repository") {
        return Err(VcsError::NotARepository(dir.display().to_string()));
    }
    Err(VcsError::Command {
        command: display,
        // git writes its diagnosis to stderr and nothing to stdout on failure; the exit code alone
        // produces "git failed: 1", which helps nobody.
        message: if message.is_empty() {
            format!("exited with {}", out.status)
        } else {
            message.to_string()
        },
    })
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub status: RepoStatus,
    pub branches: Vec<BranchInfo>,
    pub worktrees: Vec<WorktreeInfo>,
    pub recent: Vec<CommitInfo>,
    pub default_branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_args_pick_the_right_form() {
        assert_eq!(diff_args(&DiffTarget::Unstaged, false), vec!["diff"]);
        assert_eq!(diff_args(&DiffTarget::Staged, false), vec!["diff", "--cached"]);
        assert_eq!(
            diff_args(&DiffTarget::Range { base: "main".into(), head: "HEAD".into() }, true),
            vec!["diff", "main...HEAD", "--stat"],
            "three dots, not two: a pull request shows the merge-base diff"
        );
    }
}
