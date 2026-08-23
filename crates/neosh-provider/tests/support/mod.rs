//! Putting a stand-in executable on disk without racing the tests around it.
//!
//! `std::fs::write` holds the file open for writing, and every test in these binaries spawns a
//! process. A `Command::spawn` on another thread forks inside that window, the child inherits the
//! descriptor, and the `execve` that follows here fails with `ETXTBSY` — "Text file busy" — because
//! from the kernel's point of view somebody still has the file open for writing. The driver reports
//! the spawn failure, the stream ends before the handshake, and the test sees a turn that answered
//! nothing. It reproduces about one run in six on a machine with cores to spare, which is why CI
//! failed on it constantly and a laptop never did.
//!
//! So the bytes are written by a *child* process. Nothing in this process ever holds a descriptor
//! on a file it is about to execute, so a fork has nothing to inherit and the window is not merely
//! narrower — it is gone.

/// Write `contents` to `path` and make it executable, from outside this process.
pub fn write_executable(path: &std::path::Path, contents: &str) {
    let status = std::process::Command::new("/bin/sh")
        // The payload and the destination arrive as *arguments*, so nothing in a script full of
        // quotes, `$` and backslashes is ever read as shell. `$0` is the name `sh` reports.
        .args(["-c", r#"printf '%s' "$1" > "$2" && chmod 755 "$2""#, "neosh-test-write"])
        .arg(contents)
        .arg(path)
        .status()
        .expect("a shell to write the script");
    assert!(status.success(), "writing {} failed: {status}", path.display());
}
