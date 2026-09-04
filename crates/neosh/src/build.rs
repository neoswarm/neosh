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

/// The executable behind this process, resolved at startup for the same reason the stamp is.
///
/// Asked later the answer is a path ending `(deleted)` that nothing can stat or rename — and
/// renaming onto it is exactly what an update does.
static EXE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

/// The path this process was invoked by, before symlinks were followed.
///
/// Kept *unresolved* on purpose, and it is the only thing that can answer "has this install been
/// replaced". A managed install is a symlink into a versioned directory — `/opt/homebrew/bin/neosh`
/// into `Cellar/neosh/0.4.1/bin/neosh` — and upgrading it does not touch a single byte of the file
/// we are executing: it writes a new directory and repoints the link. Resolved at startup, as
/// [`exe_path`] is, that swap is invisible, because the answer was frozen on the side of it we can
/// no longer see.
static LAUNCH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

/// Where this process's executable was when it started, with every symlink already followed.
pub fn exe_path() -> Option<std::path::PathBuf> {
    EXE.get_or_init(|| std::env::current_exe().ok().and_then(|p| p.canonicalize().ok())).clone()
}

/// The path this process was invoked by, made absolute but *not* resolved.
///
/// Absolute because a relative `current_exe` is relative to the directory the process started in,
/// which is not where it stays; unresolved because following the link is what this exists to avoid.
pub fn launch_path() -> Option<std::path::PathBuf> {
    LAUNCH
        .get_or_init(|| {
            let raw = std::env::current_exe().ok()?;
            match raw.is_absolute() {
                true => Some(raw),
                false => std::env::current_dir().ok().map(|d| d.join(raw)),
            }
        })
        .clone()
}

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
