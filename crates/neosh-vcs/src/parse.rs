//! Parsers for git's machine-readable output.
//!
//! Split out from the command layer so the tricky part — and it is all tricky — is testable
//! against captured fixtures without a repository on disk.

use neosh_proto::{BranchInfo, CommitInfo, FileChange, FileState, RepoStatus, WorktreeInfo};

/// One half of the two-character `XY` status code.
fn state(c: char) -> Option<FileState> {
    match c {
        '.' => None,
        'M' => Some(FileState::Modified),
        'A' => Some(FileState::Added),
        'D' => Some(FileState::Deleted),
        'R' => Some(FileState::Renamed),
        'C' => Some(FileState::Copied),
        'T' => Some(FileState::TypeChanged),
        'U' => Some(FileState::Conflicted),
        // Anything else is a code git added after this was written. Reporting it as a modification
        // is wrong in detail but right in the only way that matters: the file is not clean.
        _ => Some(FileState::Modified),
    }
}

/// Parse `git status --porcelain=v2 --branch -z`.
///
/// `-z` is not a convenience: without it a path containing a space, a quote or a newline comes back
/// C-quoted, and every consumer has to implement git's own unquoting. With it, records are
/// NUL-separated and paths are literal.
pub fn parse_porcelain_v2(input: &str) -> RepoStatus {
    let mut status = RepoStatus::default();
    // A rename entry spends *two* NUL-separated fields: the new path, then the original. Splitting
    // naively attributes the original path to the next entry, which silently reports a phantom
    // untracked file — so the iterator is manual.
    let mut fields = input.split('\0').filter(|f| !f.is_empty());

    while let Some(record) = fields.next() {
        let mut chars = record.chars();
        let kind = chars.next().unwrap_or(' ');
        let rest = chars.as_str().trim_start();

        match kind {
            '#' => parse_header(rest, &mut status),
            '1' => {
                if let Some(change) = ordinary(rest, 7) {
                    status.changes.push(change);
                }
            }
            '2' => {
                // Field 9 is `<score>`; the path is last in *this* record and the original path is
                // the next record.
                let from = fields.next().map(str::to_string);
                if let Some(mut change) = ordinary(rest, 8) {
                    change.from = from;
                    status.changes.push(change);
                }
            }
            'u' => {
                if let Some(mut change) = ordinary(rest, 9) {
                    change.staged = Some(FileState::Conflicted);
                    change.unstaged = Some(FileState::Conflicted);
                    status.changes.push(change);
                }
            }
            '?' => status.changes.push(FileChange {
                path: rest.to_string(),
                from: None,
                staged: None,
                unstaged: Some(FileState::Untracked),
            }),
            '!' => status.changes.push(FileChange {
                path: rest.to_string(),
                from: None,
                staged: None,
                unstaged: Some(FileState::Ignored),
            }),
            _ => {}
        }
    }
    status
}

/// Shared shape of the `1`, `2` and `u` records: `<XY> <…fields…> <path>`.
///
/// `path_field` is the zero-based index of the path *within `rest`*, because the three record
/// kinds carry different numbers of mode and hash fields before it: 7 for `1`, 8 for `2` (an extra
/// similarity score) and 9 for `u` (three stages of mode and hash).
fn ordinary(rest: &str, path_field: usize) -> Option<FileChange> {
    // The path may contain spaces, so split off exactly the fixed fields and keep the remainder
    // whole. splitn with path_field + 1 leaves the tail intact.
    let mut parts = rest.splitn(path_field + 1, ' ');
    let xy = parts.next()?;
    for _ in 1..path_field {
        parts.next()?;
    }
    let path = parts.next()?;
    let mut code = xy.chars();
    let staged = state(code.next()?);
    let unstaged = state(code.next()?);
    Some(FileChange { path: path.to_string(), from: None, staged, unstaged })
}

fn parse_header(rest: &str, status: &mut RepoStatus) {
    let (key, value) = match rest.split_once(' ') {
        Some(kv) => kv,
        None => return,
    };
    match key {
        "branch.oid" => {
            if value != "(initial)" {
                // Short form is what a UI shows; git gives the full hash here.
                status.repo.head = Some(value.chars().take(8).collect());
            }
        }
        "branch.head" => {
            if value == "(detached)" {
                status.repo.detached = true;
            } else {
                status.repo.branch = Some(value.to_string());
            }
        }
        "branch.upstream" => status.repo.upstream = Some(value.to_string()),
        "branch.ab" => {
            // `+3 -1`
            for tok in value.split_whitespace() {
                let (sign, n) = tok.split_at(1);
                let n: u32 = n.parse().unwrap_or(0);
                match sign {
                    "+" => status.repo.ahead = n,
                    "-" => status.repo.behind = n,
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// One line of `git for-each-ref` in the format [`crate::Git::branches`] asks for.
pub fn branch_line(line: &str) -> Option<BranchInfo> {
    let mut f = line.split('\t');
    let name = f.next()?.to_string();
    if name.is_empty() {
        return None;
    }
    let is_head = f.next().unwrap_or("").trim() == "*";
    let upstream = f.next().unwrap_or("").trim();
    let track = f.next().unwrap_or("");
    let committed_at = f.next().unwrap_or("").trim().parse().unwrap_or(0);
    // The subject is last and may itself contain tabs, so take the remainder rather than one field.
    let subject: String = f.collect::<Vec<_>>().join("\t");

    let (ahead, behind) = parse_track(track);
    Some(BranchInfo {
        // `refs/remotes/origin/main` comes back short as `origin/main`; nothing else contains a
        // remote name before the first slash in practice, so this is how remote-ness is decided.
        is_remote: name.starts_with("origin/") || track.is_empty() && name.contains('/') && is_remote_prefixed(&name),
        name,
        is_head,
        upstream: (!upstream.is_empty()).then(|| upstream.to_string()),
        ahead,
        behind,
        subject: (!subject.is_empty()).then_some(subject),
        committed_at,
    })
}

/// `refs/remotes/<remote>/<branch>` shortens to `<remote>/<branch>`. Treat a first component that
/// looks like a remote name as remote — a local branch called `feature/x` keeps `feature` as a
/// directory component, which is why this checks a known set rather than "contains a slash".
fn is_remote_prefixed(name: &str) -> bool {
    matches!(name.split('/').next(), Some("origin" | "upstream" | "fork"))
}

/// `[ahead 3, behind 1]`, `[gone]`, or empty.
fn parse_track(track: &str) -> (u32, u32) {
    let mut ahead = 0;
    let mut behind = 0;
    let inner = track.trim().trim_start_matches('[').trim_end_matches(']');
    for part in inner.split(',') {
        let part = part.trim();
        if let Some(n) = part.strip_prefix("ahead ") {
            ahead = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            behind = n.trim().parse().unwrap_or(0);
        }
    }
    (ahead, behind)
}

/// Parse `git worktree list --porcelain`: blank-line-separated stanzas of `key value`.
pub fn parse_worktree_list(input: &str) -> Vec<WorktreeInfo> {
    let mut out: Vec<WorktreeInfo> = Vec::new();
    for stanza in input.split("\n\n") {
        let mut info = WorktreeInfo {
            path: String::new(),
            branch: None,
            head: None,
            // The first stanza is always the original checkout.
            is_main: out.is_empty(),
            locked: false,
            is_current: false,
        };
        for line in stanza.lines() {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            match key {
                "worktree" => info.path = value.to_string(),
                "HEAD" => info.head = Some(value.chars().take(8).collect()),
                "branch" => {
                    info.branch = Some(value.trim_start_matches("refs/heads/").to_string())
                }
                // `locked` may appear bare or with a reason.
                "locked" => info.locked = true,
                _ => {}
            }
        }
        if !info.path.is_empty() {
            out.push(info);
        }
    }
    out
}

/// Parse the `%H%x00%h%x00%an%x00%ct%x00%s%x00%b%x1e` log format.
pub fn commits(input: &str) -> Vec<CommitInfo> {
    input
        .split('\u{1e}')
        .map(str::trim_start)
        .filter(|r| !r.is_empty())
        .filter_map(|record| {
            let mut f = record.split('\0');
            let hash = f.next()?.trim().to_string();
            if hash.is_empty() {
                return None;
            }
            Some(CommitInfo {
                hash,
                short: f.next()?.to_string(),
                author: f.next()?.to_string(),
                committed_at: f.next()?.trim().parse().unwrap_or(0),
                subject: f.next()?.to_string(),
                body: f.next().unwrap_or("").trim_end().to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real `git status --porcelain=v2 --branch -z`, with NULs written explicitly.
    fn fixture() -> String {
        [
            "# branch.oid 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b",
            "# branch.head feature/thing",
            "# branch.upstream origin/feature/thing",
            "# branch.ab +2 -1",
            "1 M. N... 100644 100644 100644 aaaa bbbb src/staged.rs",
            "1 .M N... 100644 100644 100644 aaaa bbbb src/dirty.rs",
            "1 MM N... 100644 100644 100644 aaaa bbbb src/both.rs",
            "? untracked.txt",
        ]
        .join("\0")
            + "\0"
    }

    #[test]
    fn branch_and_ahead_behind_come_from_the_header() {
        let s = parse_porcelain_v2(&fixture());
        assert_eq!(s.repo.branch.as_deref(), Some("feature/thing"));
        assert_eq!(s.repo.upstream.as_deref(), Some("origin/feature/thing"));
        assert_eq!(s.repo.ahead, 2);
        assert_eq!(s.repo.behind, 1);
        assert!(!s.repo.detached);
        assert_eq!(s.repo.head.as_deref(), Some("1a2b3c4d"));
    }

    #[test]
    fn staged_and_unstaged_are_reported_separately() {
        let s = parse_porcelain_v2(&fixture());
        let by = |p: &str| s.changes.iter().find(|c| c.path == p).cloned().unwrap();
        assert_eq!(by("src/staged.rs").staged, Some(FileState::Modified));
        assert_eq!(by("src/staged.rs").unstaged, None);
        assert_eq!(by("src/dirty.rs").staged, None);
        assert_eq!(by("src/dirty.rs").unstaged, Some(FileState::Modified));
        // A file staged *and* then edited again must show both, or a commit UI tells the user it is
        // committing something it is not.
        assert_eq!(by("src/both.rs").staged, Some(FileState::Modified));
        assert_eq!(by("src/both.rs").unstaged, Some(FileState::Modified));
        assert_eq!(by("untracked.txt").unstaged, Some(FileState::Untracked));
        assert_eq!(s.staged_count(), 2);
    }

    #[test]
    fn a_rename_consumes_its_second_path_field() {
        // The regression this guards: the original path is a separate NUL-terminated field, so a
        // parser that splits naively reads "old/name.rs" as the next *record* and reports it as an
        // untracked file that does not exist.
        let input = [
            "# branch.head main",
            "2 R. N... 100644 100644 100644 aaaa bbbb R100 new/name.rs",
            "old/name.rs",
            "? real-untracked.txt",
        ]
        .join("\0")
            + "\0";
        let s = parse_porcelain_v2(&input);
        assert_eq!(s.changes.len(), 2, "got {:?}", s.changes);
        assert_eq!(s.changes[0].path, "new/name.rs");
        assert_eq!(s.changes[0].from.as_deref(), Some("old/name.rs"));
        assert_eq!(s.changes[0].staged, Some(FileState::Renamed));
        assert_eq!(s.changes[1].path, "real-untracked.txt");
    }

    #[test]
    fn paths_with_spaces_survive() {
        let input = ["# branch.head main", "1 .M N... 100644 100644 100644 aa bb my file name.txt"]
            .join("\0")
            + "\0";
        let s = parse_porcelain_v2(&input);
        assert_eq!(s.changes[0].path, "my file name.txt");
    }

    #[test]
    fn a_conflict_is_not_reported_as_an_ordinary_modification() {
        let input = ["# branch.head main", "u UU N... 100644 100644 100644 100644 a b c conflicted.rs"]
            .join("\0")
            + "\0";
        let s = parse_porcelain_v2(&input);
        assert_eq!(s.changes[0].path, "conflicted.rs");
        assert!(s.changes[0].is_conflicted());
    }

    #[test]
    fn detached_head_has_no_branch() {
        let s = parse_porcelain_v2(&["# branch.oid abcdef1234", "# branch.head (detached)"].join("\0"));
        assert!(s.repo.detached);
        assert_eq!(s.repo.branch, None);
    }

    #[test]
    fn a_fresh_repository_has_no_head() {
        let s = parse_porcelain_v2(&["# branch.oid (initial)", "# branch.head main"].join("\0"));
        assert_eq!(s.repo.head, None);
        assert_eq!(s.repo.branch.as_deref(), Some("main"));
    }

    #[test]
    fn branch_lines_carry_tracking_and_subject() {
        let b = branch_line("feature/x\t*\torigin/feature/x\t[ahead 3, behind 1]\t1700000000\tAdd the thing").unwrap();
        assert_eq!(b.name, "feature/x");
        assert!(b.is_head);
        assert_eq!(b.upstream.as_deref(), Some("origin/feature/x"));
        assert_eq!((b.ahead, b.behind), (3, 1));
        assert_eq!(b.subject.as_deref(), Some("Add the thing"));
        assert_eq!(b.committed_at, 1_700_000_000);
        assert!(!b.is_remote, "a local branch with a slash is not a remote branch");
    }

    #[test]
    fn a_branch_with_no_upstream_is_not_ahead_of_anything() {
        let b = branch_line("solo\t\t\t\t1700000000\tWork").unwrap();
        assert_eq!((b.ahead, b.behind), (0, 0));
        assert_eq!(b.upstream, None);
        assert!(!b.is_head);
    }

    #[test]
    fn worktrees_mark_the_first_stanza_as_main() {
        let input = "worktree /repo\nHEAD 1111111111\nbranch refs/heads/main\n\nworktree /repo-wt\nHEAD 2222222222\nbranch refs/heads/feature\nlocked in use\n";
        let t = parse_worktree_list(input);
        assert_eq!(t.len(), 2);
        assert!(t[0].is_main);
        assert_eq!(t[0].branch.as_deref(), Some("main"));
        assert!(!t[1].is_main);
        assert!(t[1].locked);
        assert_eq!(t[1].branch.as_deref(), Some("feature"));
    }

    #[test]
    fn commit_bodies_may_contain_newlines_and_tabs() {
        let input = "hash1\0h1\0Ada\01700000000\0Subject\twith tab\0Body line 1\nBody line 2\u{1e}hash2\0h2\0Bob\01700000001\0Second\0\u{1e}";
        let c = commits(input);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].subject, "Subject\twith tab");
        assert_eq!(c[0].body, "Body line 1\nBody line 2");
        assert_eq!(c[0].author, "Ada");
        assert_eq!(c[1].body, "");
    }
}
