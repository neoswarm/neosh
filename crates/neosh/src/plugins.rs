//! Installing, listing and removing plugins.
//!
//! The thesis of this program is that extensibility is the product, and until this existed there
//! was no way to *ship* an extension. `installed_plugin_dir` had a doc comment naming a plugin
//! manager that did not exist, the manifest had no way to say where a plugin came from, and the
//! instructions for installing one were "clone it into this directory yourself". An ecosystem is
//! not a plugin API; it is a plugin API plus a way for one person's plugin to end up on another
//! person's machine.
//!
//! Deliberately small, and every piece of that is a decision:
//!
//! * **Git, and nothing else.** No registry, no index, no name resolution — a URL is already a
//!   globally unique name that works for private repositories, forks and a branch you are
//!   developing on. A registry is a thing to run forever, and this is a thing to have now.
//! * **The manifest names the directory.** A plugin's id is `name` in its `plugin.toml`, so
//!   installing it anywhere else would let two directories claim one id and make which one wins
//!   depend on the order a filesystem happened to list them.
//! * **Validated at install.** [`neosh_script::manifest::load_manifest`] runs before anything is
//!   moved into place, so a plugin built against another protocol version, or missing its entry
//!   file, fails here with a sentence — rather than at the next startup, as a line in a log, in
//!   the middle of something else.
//! * **Installed is separate from hand-managed.** `$CONFIG/plugins` is yours and is never touched:
//!   `remove` refuses to delete from it, because neosh did not put it there.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, anyhow, bail};

use crate::paths::Paths;

/// Where a plugin on this machine came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Shipped inside the binary. Ordinary plugins that happen to be embedded.
    Bundled,
    /// `$CONFIG/plugins` — put there by the user, and not ours to change.
    Hand,
    /// `$DATA/plugins` — installed by `neosh plugin add`, and updatable.
    Installed,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Hand => "yours",
            Self::Installed => "installed",
        }
    }
}

pub struct Entry {
    pub name: String,
    pub version: String,
    pub origin: Origin,
    pub dir: Option<PathBuf>,
    pub permissions: Vec<neosh_proto::PluginPermission>,
    /// Where it was cloned from, for an installed one.
    pub remote: Option<String>,
}

/// Run one `git` command, letting it talk to the terminal.
///
/// The opposite of [`neosh_vcs`]'s runner, which scrubs every interactive path because a
/// credential prompt underneath a TUI is a hang with no visible cause. Here there is no TUI: this
/// is a one-shot command someone typed at a shell, and a private repository asking for a passphrase
/// is the feature rather than the bug.
fn git(dir: Option<&Path>, args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdout(Stdio::piped())
        .env("GIT_PAGER", "cat")
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                anyhow!("git is not installed, and installing a plugin is a git clone")
            }
            _ => anyhow!("could not run git: {e}"),
        })?;
    if !out.status.success() {
        bail!("git {} failed", args.first().copied().unwrap_or("command"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Clone `source` and put it where plugins are discovered.
///
/// Answers with the manifest it validated, which is what the caller prints: the name and version
/// that will actually load, read from the plugin rather than guessed from the URL.
pub fn add(paths: &Paths, source: &str) -> anyhow::Result<neosh_proto::PluginManifest> {
    if !looks_like_a_url(source) {
        // A local directory is already installable and has a better answer than copying it, which
        // would leave two copies to keep in step and no way to tell which one is being edited.
        bail!(
            "{source} is a path rather than a URL.\n\
             A plugin you are writing belongs in {} — it is discovered from there as it is, so \
             every save is live.\n\
             `plugin_dirs` in config.toml adds any other directory.",
            paths.plugin_dir().display()
        );
    }
    let root = paths.installed_plugin_dir();
    std::fs::create_dir_all(&root)
        .with_context(|| format!("could not create {}", root.display()))?;

    // Cloned beside the destination rather than into it: the name is in the manifest, which cannot
    // be read until the clone exists. A temporary directory in the same filesystem also means the
    // move into place is a rename — atomic, so a half-cloned plugin is never discoverable.
    let tmp = root.join(format!(".adding-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let scratch = Scratch(tmp.clone());
    git(None, &["clone", "--depth", "1", source, &tmp.display().to_string()])?;

    let found = neosh_script::manifest::load_manifest(&tmp)
        .map_err(|e| anyhow!("{source} is not a neosh plugin: {e}"))?;
    let name = found.manifest.name.clone();
    if name.trim().is_empty() {
        bail!("{source} has a plugin.toml with no name in it");
    }
    let dest = root.join(&name);
    if dest.exists() {
        bail!(
            "{name} is already installed.\n\
             `neosh plugin update {name}` pulls it, `neosh plugin remove {name}` takes it off."
        );
    }
    // The clone's own history is kept, because that is what `update` pulls.
    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("could not move the clone into {}", dest.display()))?;
    std::mem::forget(scratch);
    Ok(found.manifest)
}

/// A directory to delete unless somebody says otherwise.
///
/// A clone that failed validation must not be left behind: the next `add` of the same plugin would
/// find a stale temporary directory and refuse, and a directory named `.adding-4181` in the plugin
/// path is a thing nobody can interpret. Forgotten on the success path, where it has been renamed
/// into place and no longer exists under this name.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether this reads as somewhere to clone from rather than somewhere on this disk.
///
/// `scp`-style SSH remotes — `git@host:owner/repo` — are the common case for a private plugin and
/// have no scheme, so a check for `://` alone would send most private installs down the "that is a
/// path" branch with advice that does not apply.
fn looks_like_a_url(source: &str) -> bool {
    source.contains("://")
        || (source.contains(':') && !source.starts_with('.') && !source.starts_with('/'))
}

/// Every plugin this machine would load, wherever it comes from.
pub fn list(paths: &Paths) -> Vec<Entry> {
    let mut out = Vec::new();
    for p in neosh_script::loader::builtin_plugins() {
        let manifest: Option<neosh_proto::PluginManifest> = toml::from_str(&p.manifest).ok();
        out.push(Entry {
            name: p.name,
            version: manifest.as_ref().map(|m| m.version.clone()).unwrap_or_default(),
            origin: Origin::Bundled,
            dir: None,
            permissions: manifest.map(|m| m.permissions).unwrap_or_default(),
            remote: None,
        });
    }
    for (dir, origin) in
        [(paths.plugin_dir(), Origin::Hand), (paths.installed_plugin_dir(), Origin::Installed)]
    {
        let (found, _) = neosh_script::manifest::discover(&dir);
        for p in found {
            out.push(Entry {
                name: p.manifest.name.clone(),
                version: p.manifest.version.clone(),
                origin,
                remote: git(Some(&p.dir), &["remote", "get-url", "origin"]).ok(),
                dir: Some(p.dir),
                permissions: p.manifest.permissions.clone(),
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Where an installed plugin lives, refusing anything neosh did not put there.
fn installed_dir(paths: &Paths, name: &str) -> anyhow::Result<PathBuf> {
    let dir = paths.installed_plugin_dir().join(name);
    if dir.join("plugin.toml").is_file() {
        return Ok(dir);
    }
    let hand = paths.plugin_dir().join(name);
    if hand.join("plugin.toml").is_file() {
        // Deleting it would be deleting somebody's work. `add` did not put it there and this does
        // not get to take it away.
        bail!(
            "{name} is in {} — yours, not one neosh installed.\n\
             Delete the directory if you mean to.",
            hand.display()
        );
    }
    bail!("{name} is not installed. `neosh plugin list` says what is.")
}

/// Pull one installed plugin, or all of them. Answers with what moved.
pub fn update(paths: &Paths, name: Option<&str>) -> anyhow::Result<Vec<(String, String)>> {
    let names: Vec<String> = match name {
        Some(n) => vec![n.to_string()],
        None => list(paths)
            .into_iter()
            .filter(|e| e.origin == Origin::Installed)
            .map(|e| e.name)
            .collect(),
    };
    let mut moved = Vec::new();
    for n in names {
        let dir = installed_dir(paths, &n)?;
        let before = git(Some(&dir), &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        // `--ff-only`, because a plugin directory is a checkout neosh manages: a merge commit
        // written by an editor into somebody's plugin is a thing nobody asked for, and a
        // divergence is a question for a person rather than something to resolve automatically.
        git(Some(&dir), &["pull", "--ff-only"])?;
        let after = git(Some(&dir), &["rev-parse", "--short", "HEAD"]).unwrap_or_default();
        // Validated after pulling for the same reason `add` validates before moving: an upstream
        // that has moved to a protocol this build does not speak should say so now, by name,
        // rather than at the next startup.
        neosh_script::manifest::load_manifest(&dir)
            .map_err(|e| anyhow!("{n} pulled, and no longer loads: {e}"))?;
        if before != after {
            moved.push((n, format!("{before} \u{2192} {after}")));
        }
    }
    Ok(moved)
}

/// Take an installed plugin off this machine.
pub fn remove(paths: &Paths, name: &str) -> anyhow::Result<PathBuf> {
    let dir = installed_dir(paths, name)?;
    std::fs::remove_dir_all(&dir)
        .with_context(|| format!("could not remove {}", dir.display()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ssh_remote_is_a_url_and_a_relative_path_is_not() {
        // `git@host:owner/repo` has no scheme and is how a private plugin is installed, so a
        // check for `://` alone would send the common private case down the wrong branch.
        assert!(looks_like_a_url("git@github.com:someone/neosh-thing"));
        assert!(looks_like_a_url("https://github.com/someone/neosh-thing"));
        assert!(looks_like_a_url("ssh://git@host/thing.git"));
        assert!(!looks_like_a_url("./my-plugin"));
        assert!(!looks_like_a_url("/home/me/my-plugin"));
        assert!(!looks_like_a_url("my-plugin"));
    }
}
