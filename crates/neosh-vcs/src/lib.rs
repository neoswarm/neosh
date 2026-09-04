//! Git, as a set of async calls returning [`neosh_proto`] types.
//!
//! Implemented by shelling out to `git` rather than by linking a library. That is a deliberate
//! trade: `git` is already installed wherever this runs, it is the only implementation that agrees
//! with itself about worktrees, hooks, config and credential helpers, and a wrong answer from it is
//! the same wrong answer the user gets in their own terminal.
//!
//! Every call is `async` and every call is *fallible* — a directory that is not a repository is an
//! ordinary answer here, not an error to be unwrapped.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

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
    /// What git said on the way, on a command that *succeeded*.
    ///
    /// Kept because git puts some of its most useful sentences here even when nothing went wrong:
    /// `Successfully rebased and updated refs/heads/main.` is stderr, and so is every line of fetch
    /// progress. A caller that wants to report what happened has to be able to see it; on failure
    /// stderr is the error message and is carried by [`VcsError::Command`] instead.
    stderr: String,
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

    /// Hold the repository's turn at the network, until the guard is dropped.
    ///
    /// **Two git operations that talk to a remote must not overlap in one repository**, and git
    /// does not enforce it: `refs/remotes/*` is updated under a ref lock but `FETCH_HEAD` is not,
    /// and neither is the pair of them together. A fetch landing in the middle of a pull is
    ///
    /// ```text
    /// error: cannot lock ref 'refs/remotes/origin/trunk': is at 7f51353… but expected ae4cfd0…
    ///  ! ae4cfd0..7f51353  trunk -> origin/trunk  (unable to update local ref)
    /// ```
    ///
    /// — a failed pull, in a repository where nothing was wrong. That became reachable the moment
    /// something fetched on a timer: the sidebar's three-minute check against the key somebody
    /// pressed, or against a script, or against another plugin. Every one of those is a different
    /// caller, which is why the queue is *here* rather than in whichever of them was written last.
    ///
    /// Keyed by the **common** git directory, so the several worktrees of one repository queue with
    /// each other: `refs/remotes` and `FETCH_HEAD` live in the common directory and are the shared
    /// thing being written. Worth its own `rev-parse` — that is a subprocess that reads one config
    /// value, on an operation about to open a socket to another machine.
    ///
    /// Only network operations take it. `git status` on a fifty-thousand-file tree must not wait
    /// behind somebody's fetch of a large repository, and it has no reason to: it writes nothing.
    async fn network_lock(&self) -> tokio::sync::OwnedMutexGuard<()> {
        static QUEUES: OnceLock<StdMutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
            OnceLock::new();
        let key = self.common_dir().await;
        let queue = {
            let mut map = QUEUES
                .get_or_init(Default::default)
                .lock()
                // A poisoned map means some other task panicked while holding it for the length of
                // two `HashMap` operations. There is nothing to be inconsistent about, and
                // refusing every future fetch over it would be a worse outcome than the panic.
                .unwrap_or_else(|e| e.into_inner());
            Arc::clone(map.entry(key).or_default())
        };
        queue.lock_owned().await
    }

    /// The directory `refs/remotes` and `FETCH_HEAD` actually live in.
    ///
    /// `.git` in an ordinary checkout; the original repository's `.git` in a linked worktree, which
    /// is the case this exists for. Falling back to the working-tree root is safe rather than
    /// correct: it still serialises a checkout against itself, which is where the collision almost
    /// always is, and only two *different* worktrees of one repository could then still race.
    async fn common_dir(&self) -> PathBuf {
        let Ok(out) = self.git(["rev-parse", "--path-format=absolute", "--git-common-dir"]).await
        else {
            return self.root.clone();
        };
        let path = PathBuf::from(out.stdout.trim());
        if path.as_os_str().is_empty() {
            return self.root.clone();
        }
        std::fs::canonicalize(&path).unwrap_or(path)
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

    /// Move a branch to another name, keeping everything on it.
    ///
    /// `git branch -m` writes one ref and leaves the working tree alone, which is what makes it
    /// safe to run on the branch a worktree currently has checked out — including while an agent
    /// is editing files in that worktree. Renaming is therefore not a checkout: nothing is
    /// stashed, nothing is rewritten, and a half-finished edit survives it.
    ///
    /// Fails rather than clobbers when `new` is taken. `-M` would overwrite the other branch, and
    /// silently discarding somebody's work to give a generated name its first choice is not a
    /// trade worth making — the caller has the branch list and can pick a free name.
    pub async fn rename_branch(&self, old: &str, new: &str) -> Result<(), VcsError> {
        self.git(["branch", "-m", old, new]).await.map(|_| ())
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

    /// `git fetch --prune`, then where this checkout now stands.
    ///
    /// The counterpart to [`Self::status`] rather than a version of it: `status` compares HEAD with
    /// the remote-tracking ref *on this disk*, so its `behind` is only as true as the last fetch.
    /// Nothing else here talks to the remote without also changing the working tree, which is why a
    /// panel wanting to say "three waiting for you" had no way to find out.
    ///
    /// `--prune` because the alternative is a branch list that grows for ever: a remote branch
    /// deleted after its pull request merged stays in `refs/remotes` until something removes it, and
    /// a picker offering to check out six branches that no longer exist is worse than one that is a
    /// fetch behind.
    ///
    /// Tags are left alone. The default refspec already brings the ones pointing at fetched commits,
    /// and `--tags` on a timer against a repository with ten thousand of them is a lot of network
    /// for something no panel here draws.
    ///
    /// Failure is ordinary and expected: no remote, no network, no credentials. `run` closes stdin
    /// and unsets every askpass, so an unauthenticated fetch fails in a moment rather than hanging
    /// on a prompt nobody can see — which is the whole reason this is safe to put on a timer.
    pub async fn fetch(&self) -> Result<RepoStatus, VcsError> {
        let _net = self.network_lock().await;
        self.git(["fetch", "--prune"]).await?;
        self.status().await
    }

    /// `git pull`, answering with the last thing git said about it.
    ///
    /// `--no-edit` because there is no editor here to open: a pull that wants a merge message gets
    /// the default one, and a pull that wants credentials fails now rather than hanging — `run`
    /// closes stdin and forbids terminal prompts for exactly this call.
    ///
    /// `rebase` replays this branch's commits on top of what arrived instead of merging. Passed
    /// explicitly rather than left to `pull.rebase`, because the caller for it is a panel that has
    /// just told somebody their branch diverged and offered them the choice — and a choice that
    /// then does whatever the config says is not one.
    ///
    /// The summary is one line out of git's several, chosen rather than taken off the end. See
    /// [`pull_summary`], which is where the choosing is and why.
    pub async fn pull(&self, rebase: bool) -> Result<String, VcsError> {
        let _net = self.network_lock().await;
        let out = if rebase {
            // `--no-edit` has no meaning under `--rebase` and older gits complain about the pair.
            self.git(["pull", "--rebase"]).await?
        } else {
            self.git(["pull", "--no-edit"]).await?
        };
        // Both streams. A successful rebase says so on stderr and prints nothing at all on stdout,
        // so a summary read from stdout alone reported the one operation people most want confirmed
        // as `done`.
        Ok(pull_summary(&out.stdout, &out.stderr))
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
        self.git(args).await?;
        self.exclude_if_inside(path).await;
        Ok(())
    }

    /// Keep an in-tree worktree out of `git status` — in every clone.
    ///
    /// Git is happy to put a worktree inside the repository it belongs to, and then reports the
    /// whole checkout as one untracked directory, forever. The fix is a line in the repository's
    /// **`.gitignore`**, not `.git/info/exclude`: the exclude file is per clone, so a colleague —
    /// or the same person on another machine — pulls the layout and rediscovers the noise, while
    /// the tracked file says it once for everyone. Making the working tree dirty is the honest
    /// cost, and it is one line that commits with the work.
    ///
    /// The entry is the worktrees' *directory* — `/.worktrees/`, the tree's parent — because a
    /// shared file that grew a line per scratch branch would be churn everyone has to pull. A
    /// worktree sitting directly in the root, with no parent to name, gets its own directory.
    ///
    /// Nothing is written when git already ignores the path — the line from last time, a broader
    /// rule somebody wrote, a global ignore. `git check-ignore` answers that the way `git status`
    /// will, which a string search of one file cannot.
    ///
    /// Best-effort by design. The worktree already exists when this runs; failing the call over a
    /// status-noise fix would report an operation as failed after it worked.
    async fn exclude_if_inside(&self, path: &Path) {
        let Ok(out) = self.git(["rev-parse", "--path-format=absolute", "--git-common-dir"]).await
        else {
            return;
        };
        let common = PathBuf::from(out.stdout.trim());
        // "Inside the repository" means inside the *main* checkout — the common dir's parent —
        // not inside whichever worktree this command happened to run from.
        let Some(main) = common.parent().map(Path::to_path_buf) else { return };
        // Both sides resolved before they are compared. `rev-parse` answers with symlinks followed
        // — `/private/var/…` on macOS, wherever `~/Code` really lives on anybody who keeps their
        // projects on another volume — while `path` is whatever the caller was holding, which is
        // the unresolved one. Compared as written, `strip_prefix` fails, this returns quietly, and
        // the result is a repository that reports itself as one giant untracked directory for ever
        // with nothing anywhere saying why. Only the *comparison* is canonical: the line written
        // below is still relative, so what lands in `.gitignore` is `/.worktrees/` and not
        // somebody's home directory.
        let real_main = tokio::fs::canonicalize(&main).await.unwrap_or_else(|_| main.clone());
        let real_path = tokio::fs::canonicalize(path).await.unwrap_or_else(|_| path.to_path_buf());
        let Ok(rel) = real_path.strip_prefix(&real_main) else { return };
        let rel_text = rel.to_string_lossy().replace('\\', "/");
        if rel_text.is_empty() {
            return;
        }
        // Exit 0 is "ignored". Exit 1 — which `run` surfaces as an error — is "not ignored", and a
        // real failure lands on the same conservative answer: write the line.
        if run(&main, ["check-ignore", "-q", "--", rel_text.as_str()]).await.is_ok() {
            return;
        }
        let dir = match rel.parent().map(|p| p.to_string_lossy().replace('\\', "/")) {
            Some(parent) if !parent.is_empty() => parent,
            _ => rel_text,
        };
        let line = format!("/{}/", dir.trim_matches('/'));
        let gitignore = main.join(".gitignore");
        let current = tokio::fs::read_to_string(&gitignore).await.unwrap_or_default();
        if current.lines().any(|l| l.trim() == line) {
            return;
        }
        let mut next = current;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&line);
        next.push('\n');
        let _ = tokio::fs::write(&gitignore, next).await;
    }

    pub async fn remove_worktree(&self, path: &Path, force: bool) -> Result<(), VcsError> {
        let mut args = vec!["worktree".to_string(), "remove".to_string()];
        if force {
            args.push("--force".to_string());
        }
        args.push(path.display().to_string());
        self.git(args).await.map(|_| ())
    }

    /// Move a linked worktree to another directory, keeping git's own bookkeeping in step.
    ///
    /// `git worktree move` is the only correct way to do this: a worktree is a directory *and* two
    /// pointers — `.git` in the tree naming its admin directory, and `gitdir` in the admin
    /// directory naming the tree — so a `mv` leaves a checkout git reports as `prunable` and a
    /// repository that has forgotten where half of itself went.
    ///
    /// The parent is created and the leaf is not: git requires the destination itself not to
    /// exist, and refuses when its parent does not either — which for "move this into the project"
    /// is the common case, since `.worktrees/` may never have been made. Failing on a directory
    /// this call could have created would be a refusal with nothing behind it.
    ///
    /// Then [`Self::exclude_if_inside`], for the same reason [`Self::add_worktree`] runs it: a tree
    /// that has just been moved *into* the repository reports the whole checkout as one untracked
    /// directory until the repository's `.gitignore` says otherwise. Best-effort there and
    /// best-effort here — the move has already happened when it runs, and failing the call over a
    /// status-noise fix would report an operation as failed after it worked.
    ///
    /// Nothing is taken *out* of `.gitignore` on the way back out: the line names the worktrees'
    /// directory rather than this tree, its siblings may still be living there, and a shared file
    /// this edited on both directions of travel would be churn everybody has to pull.
    pub async fn move_worktree(&self, from: &Path, to: &Path) -> Result<(), VcsError> {
        if let Some(parent) = to.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await?;
            }
        }
        // The paths go through as `OsStr` rather than `display().to_string()`: a directory name is
        // not required to be UTF-8, and a lossy conversion here would hand git a path with a
        // replacement character in it and fail on the one checkout that has an odd byte in its name.
        self.git([OsStr::new("worktree"), OsStr::new("move"), from.as_os_str(), to.as_os_str()])
            .await?;
        self.exclude_if_inside(to).await;
        Ok(())
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

/// What a pull did, in one line somebody can read on a status row.
///
/// **Chosen rather than taken off the end.** This used to be the last non-empty line of stdout,
/// which is exactly right for `Already up to date.` — a one-line output — and wrong for every pull
/// that actually brought something, because git's last line there is the tail of a diffstat:
///
/// ```text
/// Updating 8e1234a..abc1234
/// Fast-forward
///  b.txt | 3 +++
///  1 file changed, 3 insertions(+)
///  create mode 100644 b.txt
/// ```
///
/// So a panel reporting a successful three-commit fast-forward said `create mode 100644 b.txt`,
/// which is true, is about one file, and is not what anybody asked. What people want is the *kind*
/// of thing that happened and *how much* of it — so this looks for the line naming the kind, joins
/// it to the diffstat line if there is one, and falls back to the first line rather than the last:
/// git leads with its summary and trails off into detail, so the front is the better guess when
/// nothing here recognises the shape.
///
/// Both streams, because a rebase reports itself on stderr and prints nothing on stdout.
fn pull_summary(stdout: &str, stderr: &str) -> String {
    // Split on carriage returns as well as newlines. Git redraws progress in place — a rebase
    // writes `Rebasing (1/1)\rSuccessfully rebased and updated refs/heads/main.` — so `lines()`
    // alone hands back one "line" with the progress counter welded to the front of the answer, and
    // a row clipped to a sidebar's width then reads `Rebasing (1/1)Succes…`.
    let lines: Vec<&str> = stdout
        .split(['\n', '\r'])
        .chain(stderr.split(['\n', '\r']))
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    // What happened, in git's own words. Matched loosely at the front of the line: the exact
    // wording of the merge line names the strategy and has changed with it (`recursive`, then
    // `ort`), and a test that pinned the whole sentence would fail on the next release.
    let kind = lines.iter().find(|l| {
        l.starts_with("Already up to date")
            || l.starts_with("Fast-forward")
            || l.starts_with("Merge made by")
            || l.starts_with("Successfully rebased")
            || l.starts_with("Current branch")
    });
    // How much of it. `git` writes this one line for every pull that changed anything.
    let stat = lines.iter().find(|l| l.contains(" changed,") || l.ends_with(" changed"));
    match (kind, stat) {
        (Some(k), Some(s)) => format!("{k} · {s}"),
        (Some(k), None) => (*k).to_string(),
        (None, Some(s)) => (*s).to_string(),
        (None, None) => lines.first().copied().unwrap_or("done").to_string(),
    }
}

/// Clone `url` into `path`, calling `on_progress` with the phase and percentage as git reports it.
///
/// A free function rather than a [`Git`] method, because it is the one call that arrives before
/// there is a repository to be a method on. It runs in `path`'s parent, which it creates: cloning
/// into a location you have just invented is the ordinary case here, not an error to hand back.
///
/// **The destination is checked before git is started.** `git clone` into an existing non-empty
/// directory fails with a message about `fatal: destination path ... already exists`, which is
/// accurate and reads, to somebody who has just pressed a key on a menu row, as though the clone
/// itself went wrong. Checked here it is a sentence naming the directory.
///
/// **Progress is `\r`-separated, not `\n`-separated**, which is why this reads bytes rather than
/// using `BufReader::lines()`: git redraws one status line in place, so a whole clone's progress
/// is a single line as far as a line reader is concerned, and the callback would fire once, at the
/// end, having reported nothing anybody could watch.
pub async fn clone(
    url: &str,
    path: &Path,
    mut on_progress: impl FnMut(&str, Option<u8>),
) -> Result<(), VcsError> {
    if std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_some()) {
        return Err(VcsError::Command {
            command: format!("clone {url}"),
            message: format!("{} already exists and is not empty", path.display()),
        });
    }
    let parent = path.parent().unwrap_or(path);
    std::fs::create_dir_all(parent)?;

    let mut cmd = Command::new("git");
    cmd.current_dir(parent)
        // `--progress` because git only reports it when stderr is a terminal, and here it is a
        // pipe. Without it the whole clone is silent and the panel spins on nothing.
        .args(["clone", "--progress", "--", url, &path.display().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C");
    // The same scrub `run` does, and for the same reason: an askpass helper opening a window, or
    // a prompt written onto a screen a TUI owns, is a hang with no visible cause. Credential
    // *helpers* are untouched, so an https clone still works wherever `git pull` already does.
    cmd.env_remove("GIT_ASKPASS");
    cmd.env_remove("SSH_ASKPASS");

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(VcsError::GitMissing),
        Err(e) => return Err(VcsError::Io(e)),
    };
    let mut stderr = child.stderr.take().expect("stderr is piped");

    // Kept whole so the failure message can be git's own. Bounded, because a clone that fails in a
    // redirect loop can write for a long time and none of it after the first screenful helps.
    let mut said = String::new();
    let mut buf = [0u8; 4096];
    let mut pending = String::new();
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut stderr, &mut buf).await?;
        if n == 0 {
            break;
        }
        pending.push_str(&String::from_utf8_lossy(&buf[..n]));
        // Both separators: `\r` is a redraw of the current phase and `\n` ends one for good, and a
        // reader that honours only the second sees one line per clone.
        while let Some(at) = pending.find(['\r', '\n']) {
            let line: String = pending.drain(..=at).collect();
            let line = line.trim_end_matches(['\r', '\n']);
            if said.len() < 8192 {
                said.push_str(line);
                said.push('\n');
            }
            if let Some((phase, percent)) = parse::clone_progress(line) {
                on_progress(&phase, percent);
            }
        }
    }
    if let Some((phase, percent)) = parse::clone_progress(&pending) {
        on_progress(&phase, percent);
    }

    let status = child.wait().await?;
    if status.success() {
        return Ok(());
    }
    // git's last word is the useful one — `Repository not found`, `Permission denied (publickey)`,
    // `could not resolve host`. Each is a different thing for the person reading to do, and an
    // exit code flattens all three into "clone failed".
    let message = said
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && parse::clone_progress(l).is_none())
        .next_back()
        .unwrap_or("clone failed")
        .to_string();
    Err(VcsError::Command { command: format!("clone {url}"), message })
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
        return Ok(Output {
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
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
    fn a_pull_is_summarised_by_what_it_did_and_not_by_its_last_line() {
        // The bug this exists for: git trails off into a diffstat, so the tail of a successful
        // fast-forward is a sentence about one file's mode bits.
        let ff = "Updating 8e1234a..abc1234\nFast-forward\n b.txt | 3 +++\n 1 file changed, \
                  3 insertions(+)\n create mode 100644 b.txt\n";
        assert_eq!(pull_summary(ff, ""), "Fast-forward · 1 file changed, 3 insertions(+)");

        // The one-line case that used to work, and still has to.
        assert_eq!(pull_summary("Already up to date.\n", ""), "Already up to date.");

        // A rebase says nothing on stdout at all, and redraws its progress in place — so the
        // sentence people want arrives welded to a counter by a carriage return rather than
        // separated from it by a newline.
        assert_eq!(
            pull_summary("", "Rebasing (1/1)\rSuccessfully rebased and updated refs/heads/main.\n"),
            "Successfully rebased and updated refs/heads/main."
        );

        // Nothing recognised falls back to the *first* line: git leads with its summary.
        assert_eq!(pull_summary("something new\nand detail below it\n", ""), "something new");
        assert_eq!(pull_summary("", ""), "done");
    }

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
