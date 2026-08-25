//! Finding out there is a newer neosh, and becoming it.
//!
//! Two halves that look like one feature and are not. *Is there something newer* is a question
//! about a registry. *May I replace this file* is a question about the machine, and it is the one
//! that decides everything: a binary under a Homebrew prefix belongs to Homebrew, and writing over
//! it leaves `brew` describing a version that is not on disk — `brew upgrade` then "succeeds" by
//! putting back the one we replaced. Same for a binary inside `node_modules`, same for one under
//! `~/.cargo/bin`. Only a binary nobody else is managing may be swapped, and everything else gets
//! told the command that would do it.
//!
//! The check is a plain HTTP GET against the releases API and is never done on the loop: it is
//! network, it can hang, and nothing on screen should wait for it. What it produces is kept, so
//! the answer to "is there an update" is instant and the *checking* is what happens in the
//! background.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use neosh_proto::{InstallMethod, UpdateOutcome, UpdateStatus};
use tokio::sync::RwLock;

/// Where releases are published. The API rather than the HTML, and `latest` rather than a list,
/// because a pre-release is not something to offer somebody who did not ask for one.
const LATEST: &str = "https://api.github.com/repos/neoswarm/neosh/releases/latest";
const DOWNLOAD: &str = "https://github.com/neoswarm/neosh/releases/download";

/// How long a check is good for. A day: the thing being watched changes on the order of weeks, and
/// a workspace can run for months.
const FRESH_FOR: Duration = Duration::from_secs(60 * 60 * 24);

/// The Rust target triple this binary was built for, which is what names the release asset.
///
/// `cfg!` rather than a runtime probe: the answer is a property of the build, and a binary that
/// guessed would download one for the wrong architecture and fail at the first exec.
const fn target_triple() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("x86_64-unknown-linux-gnu")
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some("aarch64-unknown-linux-gnu")
    } else {
        None
    }
}

/// Work out who owns the file this process is running from.
///
/// Path-shaped, because that is the only evidence there is — nothing writes a receipt. Ordered
/// most specific first: a cargo install inside a checkout is still a checkout.
pub fn install_method(exe: &Path) -> InstallMethod {
    let s = exe.to_string_lossy();
    // A `target/debug` or `target/release` binary is somebody's build, and the newest neosh is the
    // one they are about to compile. Checked first: a worktree can live anywhere, including under
    // a directory that looks like any of the others.
    if s.contains("/target/debug/") || s.contains("/target/release/") {
        return InstallMethod::Development;
    }
    if s.contains("/node_modules/") {
        return InstallMethod::Npm;
    }
    // Homebrew is `/opt/homebrew` on Apple silicon and `/usr/local` on Intel, and both put the
    // real file under `Cellar` with a symlink into `bin` — `current_exe` resolves the symlink, so
    // Cellar is what is actually seen.
    if s.contains("/Cellar/") || s.starts_with("/opt/homebrew/") || s.contains("/linuxbrew/") {
        return InstallMethod::Homebrew;
    }
    if s.contains("/.cargo/bin/") || s.contains("/.rustup/") {
        return InstallMethod::Cargo;
    }
    InstallMethod::Standalone
}

/// Compare two dotted versions numerically.
///
/// Not a string compare: `0.10.0` sorts before `0.9.0` as text, which would tell everybody running
/// 0.10 to downgrade. Anything unparseable compares as absent rather than as zero, so a tag nobody
/// anticipated cannot manufacture an update.
fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(v: &str) -> Option<Vec<u64>> {
        // A `-rc.1` suffix makes the last field unparseable, and an unparseable version is one we
        // decline to reason about rather than one we round.
        v.trim_start_matches('v')
            .split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect()
    }
    match (parts(latest), parts(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The last answer, and when it was given.
#[derive(Default)]
struct Cached {
    status: Option<UpdateStatus>,
    at: Option<SystemTime>,
    /// Set once a new binary is on disk, so the UI keeps saying "restart" rather than "update"
    /// after the download has already happened.
    restart_pending: bool,
}

/// The update checker. One per workspace.
#[derive(Clone)]
pub struct Updater {
    inner: Arc<RwLock<Cached>>,
    exe: PathBuf,
    current: String,
}

impl Updater {
    /// `exe` and `version` come from [`crate::build::capture`] — read at startup, because a
    /// workspace outlives the file it was loaded from and `current_exe` goes stale.
    pub fn new(exe: PathBuf, current: String) -> Self {
        Self { inner: Arc::new(RwLock::new(Cached::default())), exe, current }
    }

    fn method(&self) -> InstallMethod {
        install_method(&self.exe)
    }

    /// A status with no network in it, for the shape of the answer before any check has run.
    fn blank(&self, restart_pending: bool) -> UpdateStatus {
        let method = self.method();
        UpdateStatus {
            current: self.current.clone(),
            latest: None,
            behind: false,
            method,
            self_updatable: method.self_updatable(),
            upgrade_command: method.upgrade_command().map(str::to_string),
            error: None,
            restart_pending,
        }
    }

    /// What is known now, checking the network only if the last answer has gone stale or `force`.
    pub async fn check(&self, force: bool) -> UpdateStatus {
        {
            let c = self.inner.read().await;
            let fresh = c
                .at
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .is_some_and(|age| age < FRESH_FOR);
            if !force && fresh && let Some(s) = &c.status {
                return s.clone();
            }
        }

        let mut status = self.blank(self.inner.read().await.restart_pending);

        // A checkout is never told to update. Nothing published is newer than what is about to be
        // compiled, and a notice about it would fire on every developer's every start.
        if status.method == InstallMethod::Development {
            return status;
        }

        match fetch_latest().await {
            Ok(tag) => {
                status.behind = is_newer(&tag, &self.current);
                status.latest = Some(tag.trim_start_matches('v').to_string());
            }
            // Kept rather than swallowed: a failed check that reads as "up to date" is a machine
            // that never updates and never says why.
            Err(e) => status.error = Some(e),
        }

        let mut c = self.inner.write().await;
        c.status = Some(status.clone());
        c.at = Some(SystemTime::now());
        status
    }

    /// Update, by whichever route this install takes.
    pub async fn apply(&self) -> UpdateOutcome {
        let status = self.check(true).await;
        let method = status.method;

        if let Some(err) = status.error {
            return UpdateOutcome::Failed { reason: format!("could not check for updates: {err}") };
        }
        let Some(latest) = status.latest else {
            return UpdateOutcome::Failed { reason: "no published release to compare against".into() };
        };
        if !status.behind {
            return UpdateOutcome::UpToDate { version: self.current.clone() };
        }
        // A managed install is named, never touched. Running somebody's package manager for them
        // behind a keypress is how a machine ends up in a state its owner cannot explain.
        if let Some(command) = method.upgrade_command() {
            return UpdateOutcome::Delegated {
                version: latest,
                command: command.to_string(),
                method,
            };
        }
        if !method.self_updatable() {
            return UpdateOutcome::Failed {
                reason: "this install is not one neosh can replace".into(),
            };
        }

        match self.replace_binary(&latest).await {
            Ok(()) => {
                self.inner.write().await.restart_pending = true;
                UpdateOutcome::Applied { version: latest, restart_required: true }
            }
            Err(reason) => UpdateOutcome::Failed { reason },
        }
    }

    /// Download the release for this platform and swap it in.
    ///
    /// The swap is a rename onto the running path, which on Unix is atomic and is allowed while
    /// the old binary is executing — the running process keeps the inode it started with, which is
    /// exactly why the restart is a separate question. A copy-in-place would be the version of
    /// this that can leave a half-written binary behind.
    async fn replace_binary(&self, version: &str) -> Result<(), String> {
        let Some(triple) = target_triple() else {
            return Err("no published binary for this platform".into());
        };
        let name = format!("neosh-{triple}");
        let base = format!("{DOWNLOAD}/v{version}/{name}.tar.gz");

        let tar = http_bytes(&base).await.map_err(|e| format!("downloading {name}: {e}"))?;
        let want = http_text(&format!("{base}.sha256"))
            .await
            .map_err(|e| format!("downloading the checksum: {e}"))?;
        let want = want.split_whitespace().next().unwrap_or_default().to_string();

        let got = sha256_hex(&tar);
        if got != want {
            // Refused rather than installed: this is the one check standing between a release and
            // whatever else answered that URL.
            return Err(format!("checksum mismatch (expected {want}, got {got})"));
        }

        // Next to the target, because a rename across filesystems is not a rename — and /tmp is
        // very often a different filesystem from a home directory or /usr/local.
        let dir = self.exe.parent().ok_or("the running binary has no directory")?;
        let staged = dir.join(format!(".neosh-update-{version}"));
        unpack_neosh(&tar, &staged).map_err(|e| format!("unpacking: {e}"))?;

        std::fs::rename(&staged, &self.exe).map_err(|e| {
            let _ = std::fs::remove_file(&staged);
            format!("replacing {}: {e}", self.exe.display())
        })
    }
}

async fn fetch_latest() -> Result<String, String> {
    let body = http_text(LATEST).await?;
    let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "the releases API returned no tag_name".to_string())
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        // GitHub refuses a request with no user agent, and a timeout because this runs on a
        // background task that must not be able to sit forever.
        .user_agent(concat!("neosh/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())
}

async fn http_text(url: &str) -> Result<String, String> {
    let r = client()?.get(url).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("{} returned {}", url, r.status()));
    }
    r.text().await.map_err(|e| e.to_string())
}

async fn http_bytes(url: &str) -> Result<Vec<u8>, String> {
    let r = client()?.get(url).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("{} returned {}", url, r.status()));
    }
    Ok(r.bytes().await.map_err(|e| e.to_string())?.to_vec())
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Pull the `neosh` binary out of a release tarball and write it executable at `dest`.
///
/// Through the system `tar` rather than two more crates in the tree: it is on both platforms neosh
/// ships to, `install.sh` already depends on it for exactly this, and a decompressor is a large
/// thing to carry for one call. The archive is `neosh-<triple>/neosh` plus a README and a licence,
/// and `--strip-components=1` is what turns that into a flat extract.
fn unpack_neosh(tar_gz: &[u8], dest: &Path) -> std::io::Result<()> {
    use std::io::Write;

    let dir = tempdir_beside(dest)?;
    let archive = dir.join("neosh.tar.gz");
    std::fs::write(&archive, tar_gz)?;

    let out = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("--strip-components=1")
        .arg("-C")
        .arg(&dir)
        .output()?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(std::io::Error::other(format!(
            "tar failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    let extracted = dir.join("neosh");
    if !extracted.is_file() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no neosh binary in the archive",
        ));
    }

    // Copied rather than renamed out: the caller renames `dest` onto the running binary, and that
    // rename has to stay on one filesystem.
    let mut src = std::fs::File::open(&extracted)?;
    let mut out = std::fs::File::create(dest)?;
    std::io::copy(&mut src, &mut out)?;
    out.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// A scratch directory next to `near`, so everything stays on one filesystem.
fn tempdir_beside(near: &Path) -> std::io::Result<PathBuf> {
    let parent = near.parent().unwrap_or(Path::new("."));
    let dir = parent.join(format!(".neosh-unpack-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Seconds since the epoch, for stamping a check.
#[allow(dead_code)]
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_install_is_recognised_by_where_it_lives() {
        let cases = [
            ("/opt/homebrew/Cellar/neosh/0.1.0/bin/neosh", InstallMethod::Homebrew),
            ("/usr/local/Cellar/neosh/0.1.0/bin/neosh", InstallMethod::Homebrew),
            ("/home/x/.npm-global/lib/node_modules/@neosh/cli-linux-x64/bin/neosh", InstallMethod::Npm),
            ("/home/x/.cargo/bin/neosh", InstallMethod::Cargo),
            ("/home/x/.local/bin/neosh", InstallMethod::Standalone),
            ("/usr/local/bin/neosh", InstallMethod::Standalone),
            ("/home/x/src/neosh/target/debug/neosh", InstallMethod::Development),
            ("/home/x/src/neosh/target/release/neosh", InstallMethod::Development),
        ];
        for (path, want) in cases {
            assert_eq!(install_method(Path::new(path)), want, "{path}");
        }
    }

    #[test]
    fn a_checkout_wins_over_every_other_shape() {
        // A worktree can live anywhere, including under a path that looks like a managed install.
        // Getting this backwards means offering to update somebody's working copy.
        let p = Path::new("/home/x/.cargo/src/neosh/target/debug/neosh");
        assert_eq!(install_method(p), InstallMethod::Development);
    }

    #[test]
    fn versions_compare_as_numbers_not_as_text() {
        assert!(is_newer("0.10.0", "0.9.0"), "0.10 is newer than 0.9 despite sorting before it");
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn an_unparseable_version_never_manufactures_an_update() {
        // A tag nobody anticipated is declined rather than rounded — the failure mode of guessing
        // is telling every user there is an update that does not exist.
        assert!(!is_newer("0.2.0-rc.1", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("0.2.0", "some-git-build"));
    }

    #[test]
    fn only_a_standalone_binary_is_ours_to_replace() {
        assert!(InstallMethod::Standalone.self_updatable());
        for m in [InstallMethod::Homebrew, InstallMethod::Npm, InstallMethod::Cargo] {
            assert!(!m.self_updatable(), "{m:?} belongs to something else");
            assert!(m.upgrade_command().is_some(), "{m:?} must name the command that does it");
        }
        // A checkout has neither: nothing to run, and nothing to replace.
        assert!(!InstallMethod::Development.self_updatable());
        assert!(InstallMethod::Development.upgrade_command().is_none());
    }
}
