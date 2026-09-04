//! Exercises the parsers against the output of a real `git`, in a repository built for the test.
//!
//! The unit tests in `parse.rs` use captured fixtures, which stay correct only as long as git's
//! output does. These are the ones that notice when it changes.

use std::path::{Path, PathBuf};
use std::process::Command;

use neosh_proto::{DiffTarget, FileState};
use neosh_vcs::Git;

/// A throwaway repository. Removed on drop, including when a test panics.
struct Repo {
    dir: PathBuf,
}

impl Repo {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("neosh-vcs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let repo = Self { dir };
        repo.git(&["init", "--initial-branch=main"]);
        // Identity and signing are per-repo so the test cannot depend on, or disturb, the
        // developer's global git config.
        repo.git(&["config", "user.email", "test@neosh.invalid"]);
        repo.git(&["config", "user.name", "neosh test"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(&self.dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, path: &str, contents: &str) {
        let p = self.dir.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).expect("parent dir");
        }
        std::fs::write(p, contents).expect("write");
    }

    fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn status_reports_what_git_actually_prints() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("status");
    repo.write("README.md", "hello\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);

    repo.write("staged.txt", "a\n");
    repo.git(&["add", "staged.txt"]);
    repo.write("dirty.txt", "b\n");
    repo.write("README.md", "hello again\n");
    // A path with a space is the case that breaks every non-`-z` parser.
    repo.write("with space.txt", "c\n");

    let git = Git::discover(repo.path()).await.expect("discovers the repo");
    let status = git.status().await.expect("status");

    assert_eq!(status.repo.branch.as_deref(), Some("main"));
    assert!(!status.repo.detached);
    assert!(status.repo.head.is_some(), "a repo with a commit has a HEAD");

    let by = |p: &str| status.changes.iter().find(|c| c.path == p).unwrap_or_else(|| panic!("no entry for {p}, got {:?}", status.changes));
    assert_eq!(by("staged.txt").staged, Some(FileState::Added));
    assert_eq!(by("README.md").unstaged, Some(FileState::Modified));
    assert_eq!(by("dirty.txt").unstaged, Some(FileState::Untracked));
    assert_eq!(by("with space.txt").unstaged, Some(FileState::Untracked));
    assert!(status.is_dirty());
}

#[tokio::test]
async fn a_rename_keeps_both_paths() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("rename");
    repo.write("old.txt", "content that is long enough for git to score it as a rename\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);
    repo.git(&["mv", "old.txt", "new.txt"]);
    repo.write("other.txt", "untracked\n");

    let git = Git::discover(repo.path()).await.expect("repo");
    let status = git.status().await.expect("status");

    let rename = status
        .changes
        .iter()
        .find(|c| c.path == "new.txt")
        .expect("the renamed path is reported");
    assert_eq!(rename.from.as_deref(), Some("old.txt"));
    assert_eq!(rename.staged, Some(FileState::Renamed));
    // The regression: the original path must not be mistaken for a separate untracked entry.
    assert!(
        status.changes.iter().any(|c| c.path == "other.txt"),
        "the entry after a rename is still parsed"
    );
    assert_eq!(status.changes.len(), 2, "got {:?}", status.changes);
}

#[tokio::test]
async fn branches_worktrees_and_log_round_trip() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("branches");
    repo.write("a.txt", "1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "first commit\n\nWith a body."]);
    repo.git(&["switch", "-c", "feature/thing"]);
    repo.write("b.txt", "2\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "second"]);

    let git = Git::discover(repo.path()).await.expect("repo");

    let branches = git.branches(false).await.expect("branches");
    let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
    assert!(names.contains(&"main"), "got {names:?}");
    assert!(names.contains(&"feature/thing"), "got {names:?}");
    let head = branches.iter().find(|b| b.is_head).expect("one branch is HEAD");
    assert_eq!(head.name, "feature/thing");
    assert_eq!(head.subject.as_deref(), Some("second"));
    assert!(head.committed_at > 0, "committer date parsed");
    assert!(!head.is_remote);

    let log = git.log(10).await.expect("log");
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].subject, "second");
    assert_eq!(log[1].subject, "first commit");
    assert_eq!(log[1].body, "With a body.");
    assert!(!log[0].short.is_empty());

    let trees = git.worktrees().await.expect("worktrees");
    assert_eq!(trees.len(), 1);
    assert!(trees[0].is_main);
    assert!(trees[0].is_current, "the tree we ran in is the current one");

    assert_eq!(git.default_branch().await.as_deref(), Some("main"));
}

/// A worktree moved into the repository and back out again, with git still able to find it.
///
/// The reason this is a real-git test and not a parser one: `git worktree move` rewrites two
/// pointers that nothing in this crate can see — `.git` in the tree, `gitdir` in the admin
/// directory — and the only honest way to check they still agree is to ask git afterwards. A plain
/// `mv` passes every assertion about paths and leaves a checkout git calls `prunable`.
#[tokio::test]
async fn a_worktree_moves_in_and_out_of_the_repository() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("worktree-move");
    repo.write("a.txt", "1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);

    let git = Git::discover(repo.path()).await.expect("repo");
    let inside = repo.path().join(".worktrees").join("feature-thing");
    git.add_worktree(&inside, "feature/thing", true).await.expect("add worktree");

    // In the repository, so the whole checkout would otherwise read as one untracked directory.
    // The line is in the tracked `.gitignore`, not `.git/info/exclude`, so every clone gets it.
    let ignored = std::fs::read_to_string(repo.path().join(".gitignore")).unwrap_or_default();
    assert!(ignored.lines().any(|l| l.trim() == "/.worktrees/"), "got {ignored:?}");

    // Out of the repository, to a sibling — and to a parent that does not exist yet, which is the
    // case `move_worktree` creates rather than refuses.
    let outside = repo.path().parent().expect("parent").join("neosh-vcs-worktree-move-trees/thing");
    git.move_worktree(&inside, &outside).await.expect("move out");
    assert!(outside.join("a.txt").is_file(), "the checkout came with it");
    assert!(!inside.exists(), "and left nothing behind");

    let trees = git.worktrees().await.expect("worktrees");
    let moved = trees.iter().find(|t| !t.is_main).expect("the linked tree is still listed");
    assert_eq!(
        std::fs::canonicalize(&moved.path).ok(),
        std::fs::canonicalize(&outside).ok(),
        "git knows where it went",
    );
    assert_eq!(moved.branch.as_deref(), Some("feature/thing"), "on the branch it was made on");

    // And back in. Asking git from *inside the moved tree* is the half a `mv` breaks: it works
    // only if the tree's own `.git` file still points at an admin directory that points back.
    let back = repo.path().join(".worktrees").join("feature-thing");
    git.move_worktree(&outside, &back).await.expect("move back in");
    let from_tree = Git::discover(&back).await.expect("the moved tree is still a checkout");
    assert_eq!(
        from_tree.status().await.expect("status").repo.branch.as_deref(),
        Some("feature/thing"),
    );

    let _ = std::fs::remove_dir_all(outside.parent().expect("sibling dir"));
}

#[tokio::test]
async fn diff_and_commit_go_through() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("commit");
    repo.write("a.txt", "1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);

    repo.write("a.txt", "changed\n");
    let git = Git::discover(repo.path()).await.expect("repo");

    let unstaged = git.diff(&DiffTarget::Unstaged).await.expect("diff");
    assert!(unstaged.contains("changed"), "got {unstaged:?}");
    assert!(git.diff(&DiffTarget::Staged).await.expect("staged diff").is_empty());

    git.stage(&[]).await.expect("stage");
    let staged = git.diff(&DiffTarget::Staged).await.expect("staged diff");
    assert!(staged.contains("changed"));
    assert!(git.diff_stat(&DiffTarget::Staged).await.expect("stat").contains("a.txt"));

    // A message with a quote, a newline and a shell metacharacter. If any of it reached a shell,
    // this would not round-trip.
    let message = "fix: don't $(break) \"quoting\"\n\nBody line.";
    let commit = git.commit(message).await.expect("commit");
    assert_eq!(commit.subject, "fix: don't $(break) \"quoting\"");
    assert_eq!(commit.body, "Body line.");
    assert!(!git.status().await.expect("status").is_dirty());
}

#[tokio::test]
async fn a_repository_with_no_commits_is_not_an_error() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("empty");
    let git = Git::discover(repo.path()).await.expect("an empty repo is still a repo");
    let status = git.status().await.expect("status works before the first commit");
    assert_eq!(status.repo.head, None);
    assert_eq!(status.repo.branch.as_deref(), Some("main"));
    assert!(git.log(5).await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn a_plain_directory_is_not_a_repository() {
    if !have_git() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("neosh-vcs-plain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    // Not an error to unwrap — "there is no repository here" is an ordinary answer.
    assert!(Git::discover(&dir).await.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn worktrees_report_which_one_we_are_in() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("worktree");
    repo.write("a.txt", "1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);

    let extra = repo.path().parent().unwrap().join(format!(
        "neosh-vcs-worktree-extra-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&extra);

    let git = Git::discover(repo.path()).await.expect("repo");
    git.add_worktree(&extra, "side", true).await.expect("worktree add");

    let from_main = git.worktrees().await.expect("worktrees");
    assert_eq!(from_main.len(), 2, "got {from_main:?}");
    assert!(from_main.iter().any(|t| t.is_main && t.is_current));

    // Standing inside the second tree, `is_current` must move with us.
    let inside = Git::discover(&extra).await.expect("the worktree is a repo too");
    let from_side = inside.worktrees().await.expect("worktrees");
    let current = from_side.iter().find(|t| t.is_current).expect("one is current");
    assert_eq!(current.branch.as_deref(), Some("side"));
    assert!(!current.is_main);

    let _ = std::fs::remove_dir_all(&extra);
}

/// Renaming the branch a worktree is standing on moves the ref and leaves the tree alone.
///
/// The case this exists for: an agent is working in a scratch worktree, the first message has
/// arrived, and the branch is about to be given the name that message implies. If `git branch -m`
/// touched the working tree — reset it, stashed it, refused while it was dirty — the whole
/// approach would be unavailable, because the one moment there is enough information to name the
/// branch is the moment there is also uncommitted work sitting on it.
#[tokio::test]
async fn a_branch_is_renamed_under_the_worktree_standing_on_it() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("renamebranch");
    repo.write("a.txt", "1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);

    let extra = repo
        .path()
        .parent()
        .unwrap()
        .join(format!("neosh-vcs-rename-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&extra);

    let git = Git::discover(repo.path()).await.expect("repo");
    git.add_worktree(&extra, "wily-nimbus", true).await.expect("worktree add");

    // Uncommitted work in the tree, which is the realistic state at the moment of naming.
    std::fs::write(extra.join("a.txt"), "1\n2\n").expect("write");

    let inside = Git::discover(&extra).await.expect("the worktree is a repo too");
    inside.rename_branch("wily-nimbus", "fix/the-login").await.expect("rename");

    let (branch, _) = inside.identity().await;
    assert_eq!(branch.as_deref(), Some("fix/the-login"), "the tree is on the new name");
    assert_eq!(
        std::fs::read_to_string(extra.join("a.txt")).expect("read"),
        "1\n2\n",
        "and the uncommitted edit survived it"
    );

    // A name already taken is refused rather than overwritten: a generated name does not get to
    // discard somebody's branch to have its first choice.
    assert!(
        inside.rename_branch("fix/the-login", "master").await.is_err()
            || inside.rename_branch("fix/the-login", "main").await.is_err(),
        "renaming onto the default branch is refused"
    );

    let _ = std::fs::remove_dir_all(&extra);
}

/// A fetch and a pull in one repository do not fight over `refs/remotes`.
///
/// The failure this pins is not hypothetical and not rare: run the two at once by hand and git says
///
/// ```text
/// error: cannot lock ref 'refs/remotes/origin/main': is at 7f51353… but expected ae4cfd0…
///  ! ae4cfd0..7f51353  main -> origin/main  (unable to update local ref)
/// ```
///
/// and the pull exits non-zero having done nothing. It became reachable the moment anything fetched
/// on a timer — a sidebar checking every few minutes against the key somebody pressed — and it
/// presents as "pull is broken" in a repository where nothing is wrong.
#[tokio::test]
async fn a_fetch_and_a_pull_do_not_race_each_other() {
    let repo = Repo::new("netlock");
    repo.write("README.md", "hello\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);

    // A bare `origin` beside it, and a second clone playing the colleague who pushed.
    let origin = repo.path().parent().expect("parent").join("neosh-vcs-netlock-origin.git");
    let peer = repo.path().parent().expect("parent").join("neosh-vcs-netlock-peer");
    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&peer);
    repo.git(&["clone", "--bare", "--quiet", ".", &origin.to_string_lossy()]);
    repo.git(&["remote", "add", "origin", &origin.to_string_lossy()]);
    repo.git(&["fetch", "--quiet", "origin"]);
    repo.git(&["branch", "--set-upstream-to=origin/main", "main"]);

    let run = |dir: &Path, args: &[&str]| {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    run(repo.path(), &["clone", "--quiet", &origin.to_string_lossy(), &peer.to_string_lossy()]);
    run(&peer, &["config", "user.email", "peer@neosh.invalid"]);
    run(&peer, &["config", "user.name", "peer"]);
    run(&peer, &["config", "commit.gpgsign", "false"]);
    std::fs::write(peer.join("news.txt"), "from the remote\n").expect("write");
    run(&peer, &["add", "."]);
    run(&peer, &["commit", "--quiet", "-m", "news"]);
    run(&peer, &["push", "--quiet", "origin", "main"]);

    let git = Git::discover(repo.path()).await.expect("a repository");
    let other = git.clone();
    // Both at once, which is the sidebar's timer against somebody's keypress. Whichever wins, the
    // other waits — so both succeed and the commit lands.
    let (fetched, pulled) = tokio::join!(other.fetch(), git.pull(false));
    assert!(fetched.is_ok(), "the background fetch: {fetched:?}");
    assert!(pulled.is_ok(), "and the pull it overlapped: {pulled:?}");
    assert!(repo.path().join("news.txt").is_file(), "the remote's commit arrived");

    let _ = std::fs::remove_dir_all(&origin);
    let _ = std::fs::remove_dir_all(&peer);
}

/// A clone of a real repository, reporting real progress, into a directory that does not exist yet.
///
/// The last part is the case the UI actually produces: a destination is `<root>/<owner>/<repo>`
/// and nothing has ever created `<root>/<owner>`. `git clone` makes the leaf and not the path to
/// it, so a clone that only ever ran into an existing directory would work in every test and fail
/// on the first real use.
#[tokio::test]
async fn cloning_reports_progress_and_makes_its_own_parents() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("clonesrc");
    repo.write("a.txt", "1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);

    let into = repo
        .path()
        .parent()
        .unwrap()
        .join(format!("neosh-vcs-clone-{}", std::process::id()))
        // Two levels neither of which exists, which is what makes this the create_dir_all test.
        .join("owner")
        .join("repo");
    let _ = std::fs::remove_dir_all(into.parent().unwrap().parent().unwrap());

    // `file://` rather than a bare path, deliberately. A plain local clone hardlinks the object
    // store and reports nothing at all — so a test cloning `repo.path()` would exercise the
    // *parser* against an empty stream and pass whatever the streaming code did. Over `file://`
    // git runs its real transport and emits the phases an `https://` clone emits, which is the
    // thing being tested.
    let url = format!("file://{}", repo.path().display());
    let mut seen: Vec<(String, Option<u8>)> = Vec::new();
    neosh_vcs::clone(&url, &into, |phase, percent| seen.push((phase.to_string(), percent)))
        .await
        .expect("clone");

    assert!(into.join("a.txt").is_file(), "the working tree is there");
    assert!(into.join(".git").exists(), "and it is a repository");
    assert!(!seen.is_empty(), "progress was reported: {seen:?}");
    // The `remote:`-prefixed phases are the ones a two-colon line produces, and getting them back
    // under their own name is what says the prefix came off before the phase was split out.
    assert!(
        seen.iter().any(|(p, _)| p == "Counting objects"),
        "including the server's own phases, unprefixed: {seen:?}"
    );
    assert!(
        seen.iter().any(|(_, pct)| *pct == Some(100)),
        "and a percentage that reaches the end: {seen:?}"
    );

    // Discoverable afterwards, which is the whole point of having cloned it.
    let git = Git::discover(&into).await.expect("the clone is a repository");
    assert_eq!(git.root(), into.canonicalize().unwrap_or(into.clone()).as_path());

    let _ = std::fs::remove_dir_all(into.parent().unwrap().parent().unwrap());
}

/// Cloning into a directory with something in it is refused before git is started.
///
/// git's own message is about a `destination path` and reads, to somebody who pressed a key on a
/// menu row, as though the clone went wrong rather than as though the row was already taken.
#[tokio::test]
async fn cloning_onto_something_that_exists_is_refused_by_name() {
    if !have_git() {
        return;
    }
    let repo = Repo::new("cloneclash");
    repo.write("a.txt", "1\n");
    repo.git(&["add", "."]);
    repo.git(&["commit", "-m", "initial"]);

    let into = repo.path().parent().unwrap().join(format!("neosh-vcs-clash-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&into);
    std::fs::create_dir_all(&into).expect("dir");
    std::fs::write(into.join("occupied.txt"), "mine\n").expect("write");

    let err = neosh_vcs::clone(&repo.path().display().to_string(), &into, |_, _| {})
        .await
        .expect_err("refused");
    let said = err.to_string();
    assert!(said.contains(&into.display().to_string()), "it names the directory: {said}");
    assert!(
        std::fs::read_to_string(into.join("occupied.txt")).is_ok(),
        "and what was there is still there"
    );

    let _ = std::fs::remove_dir_all(&into);
}
