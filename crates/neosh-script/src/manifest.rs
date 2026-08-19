//! Plugin discovery: `plugin.toml` next to an entry module.

use std::path::{Path, PathBuf};

use neosh_proto::{PluginId, PluginManifest};

#[derive(Debug, Clone)]
pub struct DiscoveredPlugin {
    pub id: PluginId,
    pub dir: PathBuf,
    pub entry: PathBuf,
    pub manifest: PluginManifest,
}

#[derive(Debug, thiserror::Error)]
pub enum DiscoverError {
    #[error("{path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("{path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("{0}: entry file {1} does not exist")]
    MissingEntry(PathBuf, PathBuf),
    #[error(
        "{0}: plugin targets neosh plugin API v{1}, this build speaks v{2}"
    )]
    ApiMismatch(PathBuf, u32, u32),
}

/// Read one plugin directory.
pub fn load_manifest(dir: &Path) -> Result<DiscoveredPlugin, DiscoverError> {
    let path = dir.join("plugin.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|source| DiscoverError::Read { path: path.clone(), source })?;
    let manifest: PluginManifest = toml::from_str(&text)
        .map_err(|source| DiscoverError::Parse { path: path.clone(), source })?;

    if manifest.api_version != neosh_proto::PROTOCOL_VERSION {
        // Refused rather than attempted. A plugin built against a different protocol will fail in
        // confusing ways somewhere deep in a call; failing at load names the actual problem.
        return Err(DiscoverError::ApiMismatch(
            path,
            manifest.api_version,
            neosh_proto::PROTOCOL_VERSION,
        ));
    }

    let entry = dir.join(&manifest.entry);
    if !entry.is_file() {
        return Err(DiscoverError::MissingEntry(path, entry));
    }

    Ok(DiscoveredPlugin {
        id: PluginId(manifest.name.clone()),
        dir: dir.to_path_buf(),
        entry,
        manifest,
    })
}

/// Find every plugin under a directory. Each immediate subdirectory containing a `plugin.toml` is
/// one plugin.
///
/// A plugin that fails to load is reported and skipped, never fatal: one broken third-party plugin
/// must not stop the editor from starting.
pub fn discover(root: &Path) -> (Vec<DiscoveredPlugin>, Vec<DiscoverError>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (found, errors);
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("plugin.toml").is_file())
        .collect();
    // Deterministic load order so a name collision resolves the same way every run.
    dirs.sort();
    for d in dirs {
        match load_manifest(&d) {
            Ok(p) => found.push(p),
            Err(e) => errors.push(e),
        }
    }
    (found, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("neosh-manifest-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write_plugin(root: &Path, name: &str, extra: &str) -> PathBuf {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("main.ts"), "export function activate() {}").unwrap();
        std::fs::write(
            d.join("plugin.toml"),
            format!("name = \"{name}\"\nversion = \"0.1.0\"\nentry = \"main.ts\"\n{extra}"),
        )
        .unwrap();
        d
    }

    #[test]
    fn a_well_formed_plugin_loads() {
        let root = tmp("ok");
        let d = write_plugin(&root, "hello", "");
        let p = load_manifest(&d).unwrap();
        assert_eq!(p.id.0, "hello");
        assert!(p.entry.ends_with("main.ts"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn permissions_are_parsed_from_the_manifest() {
        let root = tmp("perms");
        let d = write_plugin(&root, "p", "permissions = [\"tools\", \"hooks_blocking\"]\n");
        let p = load_manifest(&d).unwrap();
        assert_eq!(p.manifest.permissions.len(), 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_protocol_mismatch_is_refused_at_load() {
        let root = tmp("ver");
        let d = write_plugin(&root, "old", "api_version = 9999\n");
        assert!(matches!(load_manifest(&d), Err(DiscoverError::ApiMismatch(..))));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_entry_file_is_reported_by_name() {
        let root = tmp("noentry");
        let d = root.join("broken");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.toml"), "name=\"b\"\nversion=\"0\"\nentry=\"nope.ts\"\n")
            .unwrap();
        assert!(matches!(load_manifest(&d), Err(DiscoverError::MissingEntry(..))));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn one_broken_plugin_does_not_hide_the_working_ones() {
        let root = tmp("mixed");
        write_plugin(&root, "aaa", "");
        write_plugin(&root, "zzz", "");
        let bad = root.join("mmm");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("plugin.toml"), "this is not toml {{{").unwrap();

        let (found, errors) = discover(&root);
        assert_eq!(found.len(), 2, "the good plugins must still load");
        assert_eq!(errors.len(), 1, "the broken one is reported, not silently skipped");
        assert_eq!(found[0].id.0, "aaa", "load order is deterministic");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_plugin_directory_is_not_an_error() {
        let (found, errors) = discover(Path::new("/nonexistent/neosh/plugins"));
        assert!(found.is_empty() && errors.is_empty());
    }
}
