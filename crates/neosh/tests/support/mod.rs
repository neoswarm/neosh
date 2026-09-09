//! What every suite's sandbox needs, in one place because nine copies of it were nine chances to
//! get it subtly wrong — and on macOS all nine did.
//!
//! Not a test target of its own: cargo compiles `tests/*.rs` as integration tests and leaves a
//! subdirectory alone, which is also why `scripts/check.sh` globbing `tests/*.rs` for suite names
//! does not pick it up.

use std::path::{Path, PathBuf};

/// Where a sandbox lives: short, and spelled the way the operating system will spell it back.
///
/// Both properties were learned from tests that failed only on macOS, and neither is about the
/// test that noticed.
///
/// **Short**, because a workspace's socket lives under here and `sun_path` is 104 bytes. macOS
/// hands `std::env::temp_dir()` a per-user directory — `/var/folders/np/4l_yfdj…/T/` — which
/// spends about fifty of them before the sandbox has named anything, so
/// `…/run/neosh/<hash>.sock` ran past the limit and `bind` failed. That took out every test in
/// `workspace.rs` at once, with a message about a socket that never appeared rather than about a
/// path that was too long.
///
/// **Canonical**, because `/tmp` and `/var` are symlinks to `/private/tmp` and `/private/var`. A
/// test that builds a path from this and compares it against one the *host* reports is comparing
/// two spellings of the same directory, and only one of them has been resolved: the host works in
/// the directory and reports what it is actually in, while the test reports what it asked for.
/// `assert_eq!(format!("cwd={}", dir.display()), …)` then fails with two paths that a person
/// reads as identical, which is the worst shape a failure can have.
///
/// `NEOSH_TEST_TMPDIR` overrides the base, for a machine where `/tmp` is not the right answer.
pub fn sandbox_root(suite: &str, name: &str) -> PathBuf {
    let base = std::env::var_os("NEOSH_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    // `nsh` rather than `neosh`, and a two-letter suite rather than its file name: every column
    // here is a column the socket path and the welcome's directory row do not get, and the pid
    // already tells two runs apart.
    let root = base.join(format!("nsh-{suite}-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("sandbox root");
    canonical(&root)
}

/// A path as the operating system will report it, or unchanged when it cannot be resolved.
///
/// Separate from [`sandbox_root`] because a test that makes a directory of its own *inside* the
/// sandbox — a second project, a worktree — has to spell that one the same way.
pub fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}
