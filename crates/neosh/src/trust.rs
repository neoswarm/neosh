//! Which project directories may run code.
//!
//! `.neosh/init.ts` in a repository is remote code execution with extra steps: you clone something,
//! start your editor in it, and it runs. Vim shipped `exrc` doing exactly that and spent two
//! decades walking it back. So neosh splits project config in two:
//!
//! * **Data applies immediately.** Picking a model (from instances *you* already configured) and
//!   *narrowing* permissions cannot execute anything or grant anything you had not already granted.
//! * **Code requires trust.** `init.ts`, extra plugin directories, new provider instances and
//!   option values wait for `neosh trust`.
//!
//! Trust is keyed on content, not path. Hashing the files means `git pull` revoking trust is
//! automatic rather than something the user has to remember, which is the entire failure mode of
//! path-only allow-lists.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How deep into `.neosh/` the digest walks.
///
/// A bound rather than unlimited recursion, so a symlink loop or a vendored `node_modules` cannot
/// turn startup into a filesystem crawl.
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StoreFile {
    #[serde(default)]
    entries: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// There is no `.neosh/` here, so there is nothing to decide.
    NoProjectConfig,
    Trusted,
    /// Never seen before.
    Untrusted,
    /// Trusted once, but the files changed since.
    Stale,
}

impl Status {
    pub fn may_run_code(&self) -> bool {
        matches!(self, Self::Trusted)
    }
}

pub struct TrustStore {
    path: PathBuf,
    file: StoreFile,
    /// `--clean`, or no writable state directory: decisions apply for this run only.
    ephemeral: bool,
}

impl TrustStore {
    /// Read the store. A corrupt store is reset rather than fatal — the safe interpretation of
    /// "unreadable trust file" is "nothing is trusted", and refusing to start would strand the user
    /// with no way to fix it from inside neosh.
    pub fn load(path: &Path, ephemeral: bool) -> Self {
        let file = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| match serde_json::from_str(&t) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!(path = %path.display(), "trust store is unreadable, treating everything as untrusted: {e}");
                    None
                }
            })
            .unwrap_or_default();
        Self { path: path.to_path_buf(), file, ephemeral }
    }

    pub fn status(&self, cwd: &Path) -> Status {
        let Some(digest) = digest_project(cwd) else { return Status::NoProjectConfig };
        match self.file.entries.get(&key(cwd)) {
            Some(e) if e.digest == digest => Status::Trusted,
            Some(_) => Status::Stale,
            None => Status::Untrusted,
        }
    }

    /// Record the current contents as trusted.
    pub fn trust(&mut self, cwd: &Path) -> anyhow::Result<()> {
        let digest = digest_project(cwd).ok_or_else(|| {
            anyhow::anyhow!("{} has no .neosh directory to trust", cwd.display())
        })?;
        self.file.entries.insert(key(cwd), Entry { digest });
        self.save()
    }

    pub fn revoke(&mut self, cwd: &Path) -> anyhow::Result<bool> {
        let removed = self.file.entries.remove(&key(cwd)).is_some();
        self.save()?;
        Ok(removed)
    }

    pub fn list(&self) -> impl Iterator<Item = &str> {
        self.file.entries.keys().map(String::as_str)
    }

    fn save(&self) -> anyhow::Result<()> {
        if self.ephemeral {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&self.file)?;
        // Write-then-rename, so an interrupted write cannot leave a truncated store that reads as
        // "nothing is trusted" *and* loses every other project.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Canonicalized where possible, so `.` and a symlinked checkout do not become two entries for one
/// project.
fn key(cwd: &Path) -> String {
    std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf()).display().to_string()
}

/// A digest over **every file** under `.neosh/`, `None` when there is no `.neosh/` at all.
///
/// Hashing only `config.toml` and `init.ts` was a hole: `init.ts` can
/// `import "./setup.ts"`, and that file would then be free to change without revoking trust —
/// which is precisely the `git pull` scenario the content hash exists to catch. Anything reachable
/// from a trusted entry point has to be covered, and the only way to be sure is to cover the
/// directory.
///
/// The relative path and the length go into the hash alongside the bytes, so moving content
/// between two files does not collide, and adding a file where there was none is a change rather
/// than being ignored.
fn digest_project(cwd: &Path) -> Option<String> {
    let dir = crate::paths::Paths::project_dir(cwd);
    if !dir.is_dir() {
        return None;
    }
    let mut files = Vec::new();
    collect(&dir, &dir, 0, &mut files);
    if files.is_empty() {
        return None;
    }
    // Sorted, so the digest does not depend on directory iteration order.
    files.sort();

    let mut h = Sha256::new();
    for rel in files {
        h.update(rel.as_bytes());
        h.update([0]);
        match std::fs::read(dir.join(&rel)) {
            Ok(bytes) => {
                h.update(bytes.len().to_le_bytes());
                h.update(&bytes);
            }
            // Unreadable contributes a marker rather than nothing: a file that becomes readable
            // later is a change.
            Err(_) => h.update(b"-"),
        }
        h.update([0]);
    }
    Some(format!("{:x}", h.finalize()))
}

/// Every file trust covers for this workspace, relative to `.neosh/`.
///
/// Public so `neosh trust` can show the user exactly what they are approving.
pub fn list_covered(cwd: &Path, out: &mut Vec<String>) {
    let dir = crate::paths::Paths::project_dir(cwd);
    collect(&dir, &dir, 0, out);
}

/// Relative paths of every regular file under `root`, using `/` separators on every platform.
fn collect(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // `symlink_metadata`, so a symlink is judged as a link rather than followed out of the
        // directory — and so a loop cannot be walked forever.
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if meta.is_dir() {
            collect(root, &path, depth + 1, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!(
            "neosh-trust-{}-{name}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join(".neosh")).unwrap();
        Tmp(p)
    }

    fn store(root: &Path) -> TrustStore {
        TrustStore::load(&root.join("trust.json"), false)
    }

    #[test]
    fn a_directory_without_project_config_needs_no_decision() {
        let d = std::env::temp_dir();
        let s = TrustStore::load(&d.join("neosh-nonexistent-trust.json"), true);
        assert_eq!(s.status(Path::new("/")), Status::NoProjectConfig);
    }

    #[test]
    fn a_new_project_is_untrusted() {
        let t = tmp("new");
        std::fs::write(t.0.join(".neosh/init.ts"), "export function activate() {}").unwrap();
        assert_eq!(store(&t.0).status(&t.0), Status::Untrusted);
    }

    #[test]
    fn trusting_then_reading_back_reports_trusted() {
        let t = tmp("roundtrip");
        std::fs::write(t.0.join(".neosh/init.ts"), "export function activate() {}").unwrap();
        let mut s = store(&t.0);
        s.trust(&t.0).unwrap();
        assert!(s.status(&t.0).may_run_code());
        // A fresh load sees the same decision: trust survives a restart.
        assert_eq!(store(&t.0).status(&t.0), Status::Trusted);
    }

    #[test]
    fn editing_a_trusted_file_revokes_trust() {
        // The whole reason trust is keyed on content: `git pull` must not silently keep running.
        let t = tmp("edited");
        std::fs::write(t.0.join(".neosh/init.ts"), "export function activate() {}").unwrap();
        let mut s = store(&t.0);
        s.trust(&t.0).unwrap();
        std::fs::write(t.0.join(".neosh/init.ts"), "export function activate() { evil() }").unwrap();
        assert_eq!(s.status(&t.0), Status::Stale);
        assert!(!s.status(&t.0).may_run_code());
    }

    #[test]
    fn adding_a_second_file_after_trusting_also_revokes() {
        let t = tmp("added");
        std::fs::write(t.0.join(".neosh/config.toml"), "model = \"a/b\"\n").unwrap();
        let mut s = store(&t.0);
        s.trust(&t.0).unwrap();
        std::fs::write(t.0.join(".neosh/init.ts"), "export function activate() {}").unwrap();
        assert_eq!(s.status(&t.0), Status::Stale, "a new init.ts must not inherit trust");
    }

    #[test]
    fn revoking_takes_effect() {
        let t = tmp("revoke");
        std::fs::write(t.0.join(".neosh/init.ts"), "export function activate() {}").unwrap();
        let mut s = store(&t.0);
        s.trust(&t.0).unwrap();
        assert!(s.revoke(&t.0).unwrap());
        assert_eq!(s.status(&t.0), Status::Untrusted);
        assert!(!s.revoke(&t.0).unwrap(), "revoking twice reports that there was nothing to do");
    }

    #[test]
    fn a_corrupt_store_trusts_nothing_rather_than_refusing_to_start() {
        let t = tmp("corrupt");
        std::fs::write(t.0.join(".neosh/init.ts"), "export function activate() {}").unwrap();
        std::fs::write(t.0.join("trust.json"), "{{{ not json").unwrap();
        assert_eq!(store(&t.0).status(&t.0), Status::Untrusted);
    }

    #[test]
    fn a_module_the_config_imports_is_covered_by_trust() {
        // The hole this closes: `.neosh/init.ts` importing `./setup.ts` meant `setup.ts` could be
        // rewritten by a `git pull` without ever revoking trust.
        let t = tmp("imports");
        std::fs::write(t.0.join(".neosh/init.ts"), "import './setup.ts';").unwrap();
        std::fs::write(t.0.join(".neosh/setup.ts"), "export const x = 1;").unwrap();
        let mut s = store(&t.0);
        s.trust(&t.0).unwrap();
        assert!(s.status(&t.0).may_run_code());

        std::fs::write(t.0.join(".neosh/setup.ts"), "export const x = evil();").unwrap();
        assert_eq!(s.status(&t.0), Status::Stale, "editing an imported module must revoke trust");
    }

    #[test]
    fn a_file_in_a_subdirectory_is_covered_too() {
        let t = tmp("nested");
        std::fs::create_dir_all(t.0.join(".neosh/lib")).unwrap();
        std::fs::write(t.0.join(".neosh/init.ts"), "import './lib/a.ts';").unwrap();
        std::fs::write(t.0.join(".neosh/lib/a.ts"), "export const a = 1;").unwrap();
        let mut s = store(&t.0);
        s.trust(&t.0).unwrap();
        std::fs::write(t.0.join(".neosh/lib/a.ts"), "export const a = 2;").unwrap();
        assert_eq!(s.status(&t.0), Status::Stale);
    }

    #[test]
    fn moving_a_file_between_directories_changes_the_digest() {
        // The path is hashed, not just the bytes, so relocating content is a change.
        let t = tmp("moved");
        std::fs::create_dir_all(t.0.join(".neosh/lib")).unwrap();
        std::fs::write(t.0.join(".neosh/a.ts"), "x").unwrap();
        let before = digest_project(&t.0).unwrap();
        std::fs::remove_file(t.0.join(".neosh/a.ts")).unwrap();
        std::fs::write(t.0.join(".neosh/lib/a.ts"), "x").unwrap();
        assert_ne!(before, digest_project(&t.0).unwrap());
    }

    #[test]
    fn moving_content_between_files_changes_the_digest() {
        let t = tmp("swap");
        std::fs::write(t.0.join(".neosh/config.toml"), "x").unwrap();
        let a = digest_project(&t.0).unwrap();
        std::fs::remove_file(t.0.join(".neosh/config.toml")).unwrap();
        std::fs::write(t.0.join(".neosh/init.ts"), "x").unwrap();
        assert_ne!(a, digest_project(&t.0).unwrap());
    }

    #[test]
    fn an_ephemeral_store_never_writes() {
        let t = tmp("ephemeral");
        std::fs::write(t.0.join(".neosh/init.ts"), "export function activate() {}").unwrap();
        let path = t.0.join("trust.json");
        let mut s = TrustStore::load(&path, true);
        s.trust(&t.0).unwrap();
        assert!(s.status(&t.0).may_run_code(), "the decision holds for this run");
        assert!(!path.exists(), "--clean must not touch the user's real trust store");
    }
}
