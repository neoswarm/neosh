//! Where HEAD is, read off the repository's files.
//!
//! The one thing in this crate that does not shell out, and the reason is the clock it runs on.
//! Everything else here answers a key somebody pressed; this answers a panel *redrawing* — whether
//! the copy of a repository on another machine is the version this one has, asked for every
//! project row every few seconds and again for every peer's inventory. A `git` process apiece is
//! a fork per row per frame. The files are three small reads.
//!
//! What it understands is exactly what `git` itself writes for the question it answers: a `.git`
//! directory or a `gitdir:` file (a linked worktree, a submodule), `HEAD` as a symbolic ref or a
//! bare hash, the branch as a loose ref in the worktree's own directory or the common one, and
//! `packed-refs` for a branch nobody has committed to since the last `git gc`. Anything past that —
//! a reftable repository, a ref stored somewhere exotic — reads as "cannot tell", which every
//! caller already has to handle for a directory that is not a repository at all.

use std::path::{Path, PathBuf};

use neosh_proto::GitHead;

/// How far above `dir` to look for the repository it is in. A project is nearly always its own
/// root; this is for the one opened on a subdirectory, and a bound so a path on a slow network
/// mount does not stat its way to `/`.
const MAX_ASCENT: usize = 32;

/// How many symbolic refs to follow before deciding it is a loop.
const MAX_HOPS: usize = 5;

/// Where HEAD is at `dir`. `None` when `dir` is not inside a repository this can read.
pub fn read_head(dir: &Path) -> Option<GitHead> {
    let (git_dir, common) = locate(dir)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(mut name) = head.strip_prefix("ref:").map(|r| r.trim().to_string()) else {
        // Detached: the file *is* the commit.
        return Some(GitHead { branch: None, commit: is_hash(head).then(|| head.to_string()) });
    };
    let branch = name.strip_prefix("refs/heads/").map(str::to_string);
    for _ in 0..MAX_HOPS {
        match resolve(&git_dir, &common, &name) {
            Some(Ref::Commit(commit)) => return Some(GitHead { branch, commit: Some(commit) }),
            Some(Ref::Symbolic(next)) => name = next,
            // A branch with no commits yet — `git init` and nothing since — is a repository on a
            // branch, and that is worth saying even though there is nothing to point at.
            None => break,
        }
    }
    Some(GitHead { branch, commit: None })
}

/// What one ref said.
enum Ref {
    Commit(String),
    Symbolic(String),
}

/// A ref's value: the worktree's own directory first, then the shared one, then the packed file.
///
/// In that order because it is the order `git` looks in: a linked worktree keeps `HEAD` and its
/// per-worktree refs beside it and everything under `refs/heads` in the common directory, and a
/// loose ref always beats a packed one.
fn resolve(git_dir: &Path, common: &Path, name: &str) -> Option<Ref> {
    for base in [git_dir, common] {
        if let Ok(text) = std::fs::read_to_string(base.join(name)) {
            let text = text.trim();
            if let Some(next) = text.strip_prefix("ref:") {
                return Some(Ref::Symbolic(next.trim().to_string()));
            }
            if is_hash(text) {
                return Some(Ref::Commit(text.to_string()));
            }
        }
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        // `# pack-refs with: …` is a header and `^<hash>` is the peeled tag above it; neither is
        // a ref of its own.
        let (hash, refname) = line.split_once(' ')?;
        (refname.trim() == name && is_hash(hash)).then(|| Ref::Commit(hash.to_string()))
    })
}

/// The repository's own directory and its common one, for the working tree `dir` is in.
fn locate(dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut at = Some(dir);
    for _ in 0..MAX_ASCENT {
        let here = at?;
        let dot = here.join(".git");
        if dot.is_dir() {
            let common = common_of(&dot);
            return Some((dot, common));
        }
        if dot.is_file() {
            // A linked worktree or a submodule: one line naming where the real directory is,
            // relative to the file when it is not absolute.
            let text = std::fs::read_to_string(&dot).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let git_dir = here.join(target);
            let common = common_of(&git_dir);
            return Some((git_dir, common));
        }
        at = here.parent();
    }
    None
}

/// Where a repository directory keeps what it shares with its worktrees: itself, unless a
/// `commondir` file says otherwise.
fn common_of(git_dir: &Path) -> PathBuf {
    match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => git_dir.join(text.trim()),
        Err(_) => git_dir.to_path_buf(),
    }
}

/// Forty hex digits, or sixty-four in a SHA-256 repository.
fn is_hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(name: &str) -> Tmp {
        let dir = std::env::temp_dir().join(format!("neosh-head-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("tmp");
        Tmp(dir)
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo(t: &Tmp) -> PathBuf {
        let dir = t.0.join("repo");
        std::fs::create_dir_all(&dir).expect("repo");
        git(&dir, &["init", "-q", "--initial-branch=trunk"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
        dir
    }

    #[test]
    fn a_branch_is_read_as_the_branch_and_the_commit_it_points_at() {
        let t = tmp("branch");
        let dir = repo(&t);
        let head = read_head(&dir).expect("a repository");
        assert_eq!(head.branch.as_deref(), Some("trunk"));
        assert_eq!(head.commit, Some(git(&dir, &["rev-parse", "HEAD"])));
        // From a subdirectory too: a project opened one level down is still in the repository.
        let sub = dir.join("src");
        std::fs::create_dir_all(&sub).expect("sub");
        assert_eq!(read_head(&sub), Some(head));
    }

    #[test]
    fn a_packed_branch_is_found_where_gc_put_it() {
        let t = tmp("packed");
        let dir = repo(&t);
        git(&dir, &["pack-refs", "--all"]);
        assert!(!dir.join(".git/refs/heads/trunk").exists(), "the ref should now be packed");
        let head = read_head(&dir).expect("a repository");
        assert_eq!(head.commit, Some(git(&dir, &["rev-parse", "HEAD"])));
    }

    #[test]
    fn a_linked_worktree_reads_its_own_head_and_the_shared_refs() {
        let t = tmp("worktree");
        let dir = repo(&t);
        let tree = t.0.join("tree");
        git(&dir, &["worktree", "add", "-q", "-b", "fix/thing", tree.to_str().expect("utf-8")]);
        git(&tree, &["commit", "-q", "--allow-empty", "-m", "two"]);
        let head = read_head(&tree).expect("a worktree");
        assert_eq!(head.branch.as_deref(), Some("fix/thing"));
        assert_eq!(head.commit, Some(git(&tree, &["rev-parse", "HEAD"])));
        // And the main checkout is not moved by it: two checkouts, two versions.
        assert_ne!(read_head(&dir).expect("repo").commit, head.commit);
    }

    #[test]
    fn a_detached_head_has_a_commit_and_no_branch() {
        let t = tmp("detached");
        let dir = repo(&t);
        let commit = git(&dir, &["rev-parse", "HEAD"]);
        git(&dir, &["checkout", "-q", "--detach"]);
        assert_eq!(read_head(&dir), Some(GitHead { branch: None, commit: Some(commit) }));
    }

    #[test]
    fn nothing_committed_is_a_branch_with_no_commit_and_no_repository_is_none() {
        let t = tmp("empty");
        let dir = t.0.join("empty");
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(read_head(&dir), None);
        git(&dir, &["init", "-q", "--initial-branch=trunk"]);
        assert_eq!(
            read_head(&dir),
            Some(GitHead { branch: Some("trunk".into()), commit: None })
        );
    }
}
