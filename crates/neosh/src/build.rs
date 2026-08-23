//! Which build of neosh this process is running.
//!
//! The workspace is a process and the terminal is a viewer of it, so the two are not obliged to be
//! the same program — and during development they routinely are not. `cargo run` replaces
//! `target/debug/neosh` and then *attaches to the workspace already serving this config
//! directory*, which is still executing the binary it started with. Every plugin runs there, the
//! sidebar included, so a rebuild that changed how a panel draws changes nothing on screen: the
//! code that draws it is in another process, and the file it was loaded from no longer exists.
//!
//! `PROTOCOL_VERSION` already refuses the case where the two cannot talk. This is the other one,
//! which is worse for being survivable: they talk perfectly, and you are looking at last night's
//! code with no indication that you are.

use std::sync::OnceLock;

use neosh_proto::BuildId;

/// This process's build, captured the first time it is asked for.
///
/// Call [`capture`] at startup rather than relying on that: a workspace outlives the file it was
/// loaded from — `cargo run` unlinks it and writes a new one at the same path — and after that
/// `current_exe` is a path with `(deleted)` on the end which nothing can stat. Asked early the
/// answer is the truth; asked late it is nothing at all, and a workspace that cannot say what it
/// is is one nobody can be warned about.
static ID: OnceLock<BuildId> = OnceLock::new();

/// Read this process's build identity now, while its executable is still on disk.
pub fn capture() -> &'static BuildId {
    ID.get_or_init(|| BuildId {
        version: env!("CARGO_PKG_VERSION").to_string(),
        written: written_at().unwrap_or(0),
    })
}

/// When the executable behind this process was written, in seconds since the epoch.
///
/// Not "when it was compiled": a release you downloaded is stamped when it landed on disk. What it
/// is for is telling two builds apart and saying *which of them is behind*, and a modification
/// time does both whatever wrote it. `None` when the executable cannot be found or stat'd, which
/// is reported as an unknown build rather than guessed at — see [`BuildId::comparable`].
fn written_at() -> Option<u64> {
    let exe = std::env::current_exe().ok()?;
    let meta = std::fs::metadata(exe).ok()?;
    let mtime = meta.modified().ok()?;
    Some(mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs())
}
