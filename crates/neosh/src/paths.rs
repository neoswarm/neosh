//! Where neosh keeps things.
//!
//! Four roots, following the XDG base directory spec, because a single `~/.neosh` mixing
//! hand-edited config with a plugin cache is a directory nobody can back up correctly:
//!
//! | Root | Holds | Safe to delete |
//! |---|---|---|
//! | config | `init.ts`, `config.toml`, `plugins/` | no — this is what you wrote |
//! | data   | plugins a manager installed | no, but it is reproducible |
//! | state  | the trust store | mostly — you re-trust your projects |
//! | cache  | derived files | yes, always |
//!
//! Every root is overridable by environment variable. That is not a convenience: the end-to-end
//! tests need a config directory that is not the developer's own, and a config system you cannot
//! point somewhere else is a config system you cannot test.

use std::path::{Path, PathBuf};

/// The resolved directory set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    /// True when `--clean` was passed: nothing is read from disk and nothing is written.
    pub clean: bool,
}

impl Paths {
    /// Resolve from the environment, with `--config-dir` overriding the config root.
    pub fn resolve(config_override: Option<PathBuf>, clean: bool) -> Self {
        let dirs = directories::ProjectDirs::from("", "", "neosh");

        let config = config_override
            .or_else(|| env_dir("NEOSH_CONFIG_DIR"))
            .or_else(|| dirs.as_ref().map(|d| d.config_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from(".neosh-config"));

        let data = env_dir("NEOSH_DATA_DIR")
            .or_else(|| dirs.as_ref().map(|d| d.data_dir().to_path_buf()))
            .unwrap_or_else(|| config.join("data"));

        // `state_dir` is Linux-only in `directories`; elsewhere state lives with data, which is
        // what every other tool on those platforms does.
        let state = env_dir("NEOSH_STATE_DIR")
            .or_else(|| dirs.as_ref().and_then(|d| d.state_dir().map(Path::to_path_buf)))
            .unwrap_or_else(|| data.clone());

        let cache = env_dir("NEOSH_CACHE_DIR")
            .or_else(|| dirs.as_ref().map(|d| d.cache_dir().to_path_buf()))
            .unwrap_or_else(|| config.join("cache"));

        Self { config, data, state, cache, clean }
    }

    /// The user's config script. This is neosh's `init.lua`.
    pub fn init_script(&self) -> PathBuf {
        self.config.join("init.ts")
    }

    /// The declarative layer, read before any code runs.
    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    /// Plugins the user dropped in by hand. A plugin manager installs into `data` instead.
    pub fn plugin_dir(&self) -> PathBuf {
        self.config.join("plugins")
    }

    /// Where a future plugin manager installs. Searched after the hand-managed directory so a
    /// local copy always wins over an installed one.
    pub fn installed_plugin_dir(&self) -> PathBuf {
        self.data.join("plugins")
    }

    /// The `@neosh/api` source tree, written next to the config so `tsc` can resolve it.
    ///
    /// Emitted from the binary rather than fetched, so the types always describe the neosh that is
    /// actually running. `tsconfig.json` maps `@neosh/api` here.
    pub fn api_types(&self) -> PathBuf {
        self.config.join("types")
    }

    pub fn tsconfig(&self) -> PathBuf {
        self.config.join("tsconfig.json")
    }

    /// Which project directories the user has approved for code execution.
    pub fn trust_store(&self) -> PathBuf {
        self.state.join("trust.json")
    }

    /// Project-local config, relative to a workspace root.
    pub fn project_dir(cwd: &Path) -> PathBuf {
        cwd.join(".neosh")
    }

    pub fn project_config(cwd: &Path) -> PathBuf {
        Self::project_dir(cwd).join("config.toml")
    }

    pub fn project_init(cwd: &Path) -> PathBuf {
        Self::project_dir(cwd).join("init.ts")
    }
}

/// An env var naming a directory. Empty means "unset", so `NEOSH_CONFIG_DIR=` in a wrapper script
/// falls through to the default instead of resolving to the current directory.
fn env_dir(key: &str) -> Option<PathBuf> {
    let raw = std::env::var_os(key)?;
    if raw.is_empty() {
        return None;
    }
    let s = raw.to_string_lossy().into_owned();
    Some(PathBuf::from(shellexpand::tilde(&s).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_config_dir_wins_over_the_environment() {
        let p = Paths::resolve(Some(PathBuf::from("/tmp/explicit")), false);
        assert_eq!(p.config, PathBuf::from("/tmp/explicit"));
        assert_eq!(p.init_script(), PathBuf::from("/tmp/explicit/init.ts"));
    }

    #[test]
    fn the_config_layout_is_what_the_docs_promise() {
        let p = Paths::resolve(Some(PathBuf::from("/c")), false);
        assert_eq!(p.config_file(), PathBuf::from("/c/config.toml"));
        assert_eq!(p.plugin_dir(), PathBuf::from("/c/plugins"));
        assert_eq!(p.tsconfig(), PathBuf::from("/c/tsconfig.json"));
        assert_eq!(p.api_types(), PathBuf::from("/c/types"));
    }

    #[test]
    fn project_paths_hang_off_the_workspace_root() {
        let cwd = Path::new("/work/repo");
        assert_eq!(Paths::project_config(cwd), PathBuf::from("/work/repo/.neosh/config.toml"));
        assert_eq!(Paths::project_init(cwd), PathBuf::from("/work/repo/.neosh/init.ts"));
    }

    #[test]
    fn an_empty_env_override_is_treated_as_unset() {
        // A wrapper script exporting an empty var must not silently relocate the config to `.`.
        // SAFETY: single-threaded test, and the value is removed before returning.
        unsafe { std::env::set_var("NEOSH_TEST_EMPTY_DIR", "") };
        assert_eq!(env_dir("NEOSH_TEST_EMPTY_DIR"), None);
        unsafe { std::env::remove_var("NEOSH_TEST_EMPTY_DIR") };
    }
}
