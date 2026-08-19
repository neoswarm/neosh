//! What a plugin remembers between runs.
//!
//! One JSON object per plugin under `$STATE/plugin-state/<plugin>.json`. It exists because the
//! script sandbox has no filesystem — deliberately — and yet a panel that lets you pin a project
//! has to still be pinned tomorrow. Options are the wrong home for that: an option is configuration
//! the *user* writes, and an editor that rewrites someone's config file because they pressed a key
//! is an editor you stop trusting with the file.
//!
//! Keyed by the calling plugin, which the host knows and the caller cannot forge, so one plugin
//! cannot read or overwrite another's state. Nothing secret belongs here — it is plain JSON on
//! disk, the same as the rest of `$STATE`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{Map, Value};

/// Everything loaded so far, one entry per plugin.
///
/// Read-through and write-through: a `get` after the first touch costs nothing, and a `set` is on
/// disk before it is acknowledged. A panel redrawing four times a second must not mean four file
/// reads on the host loop.
#[derive(Debug, Default)]
pub struct PluginState {
    dir: Option<PathBuf>,
    loaded: HashMap<String, Map<String, Value>>,
}

impl PluginState {
    pub fn new(state_dir: Option<PathBuf>) -> Self {
        Self { dir: state_dir.map(|d| d.join("plugin-state")), loaded: HashMap::new() }
    }

    pub fn get(&mut self, plugin: &str, key: &str) -> Value {
        self.entry(plugin).get(key).cloned().unwrap_or(Value::Null)
    }

    pub fn set(&mut self, plugin: &str, key: String, value: Value) {
        self.entry(plugin).insert(key, value);
        self.flush(plugin);
    }

    pub fn delete(&mut self, plugin: &str, key: &str) {
        self.entry(plugin).remove(key);
        self.flush(plugin);
    }

    fn entry(&mut self, plugin: &str) -> &mut Map<String, Value> {
        if !self.loaded.contains_key(plugin) {
            let loaded = self
                .path(plugin)
                .and_then(|p| std::fs::read(p).ok())
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| match v {
                    Value::Object(m) => Some(m),
                    // A file that is valid JSON but not an object is someone else's; treating it as
                    // empty loses nothing we wrote and is better than refusing to start.
                    _ => None,
                })
                .unwrap_or_default();
            self.loaded.insert(plugin.to_string(), loaded);
        }
        self.loaded.get_mut(plugin).expect("just inserted")
    }

    /// Written to a temporary file and renamed, so a crash mid-write leaves yesterday's pins rather
    /// than half a file that fails to parse and silently loses all of them.
    fn flush(&mut self, plugin: &str) {
        let Some(path) = self.path(plugin) else { return };
        let Some(map) = self.loaded.get(plugin) else { return };
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let json = serde_json::to_vec_pretty(map)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, &path)
        };
        if let Err(e) = write() {
            tracing::warn!(%plugin, "could not save plugin state: {e}");
        }
    }

    /// `None` when there is no state directory, or when the plugin id is not something we are
    /// willing to put in a path. A plugin called `../../.ssh/authorized_keys` gets an in-memory
    /// store for the session and nothing on disk.
    fn path(&self, plugin: &str) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        if !is_safe_id(plugin) {
            return None;
        }
        Some(dir.join(format!("{plugin}.json")))
    }
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp(name: &str) -> Tmp {
        let p = std::env::temp_dir().join(format!("neosh-pstate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("tmp");
        Tmp(p)
    }

    #[test]
    fn a_value_survives_a_restart() {
        let t = tmp("restart");
        let mut s = PluginState::new(Some(t.0.clone()));
        s.set("sidebar", "favorites".into(), json!(["/a", "/b"]));

        let mut fresh = PluginState::new(Some(t.0.clone()));
        assert_eq!(fresh.get("sidebar", "favorites"), json!(["/a", "/b"]));
    }

    #[test]
    fn one_plugin_cannot_see_anothers_state() {
        let t = tmp("isolated");
        let mut s = PluginState::new(Some(t.0.clone()));
        s.set("sidebar", "pinned".into(), json!(1));
        assert_eq!(s.get("other", "pinned"), Value::Null);
    }

    #[test]
    fn a_missing_key_reads_as_null() {
        let mut s = PluginState::new(None);
        assert_eq!(s.get("sidebar", "nothing"), Value::Null);
    }

    #[test]
    fn delete_removes_it_from_disk_too() {
        let t = tmp("delete");
        let mut s = PluginState::new(Some(t.0.clone()));
        s.set("sidebar", "k".into(), json!("v"));
        s.delete("sidebar", "k");

        let mut fresh = PluginState::new(Some(t.0.clone()));
        assert_eq!(fresh.get("sidebar", "k"), Value::Null);
    }

    #[test]
    fn a_plugin_id_that_is_not_a_plain_identifier_never_reaches_the_filesystem() {
        let t = tmp("traversal");
        let mut s = PluginState::new(Some(t.0.clone()));
        s.set("../escape", "k".into(), json!("v"));

        assert!(!t.0.parent().expect("parent").join("escape.json").exists());
        // Still usable for the session, just not persisted: refusing outright would make a badly
        // named plugin crash instead of merely forget.
        assert_eq!(s.get("../escape", "k"), json!("v"));
    }

    #[test]
    fn a_corrupt_file_reads_as_empty_rather_than_failing() {
        let t = tmp("corrupt");
        std::fs::create_dir_all(t.0.join("plugin-state")).expect("dir");
        std::fs::write(t.0.join("plugin-state/sidebar.json"), b"{not json").expect("write");

        let mut s = PluginState::new(Some(t.0.clone()));
        assert_eq!(s.get("sidebar", "anything"), Value::Null);
        s.set("sidebar", "k".into(), json!(2));
        assert_eq!(s.get("sidebar", "k"), json!(2));
    }

    #[test]
    fn writes_are_atomic() {
        let t = tmp("atomic");
        let mut s = PluginState::new(Some(t.0.clone()));
        s.set("sidebar", "k".into(), json!("v"));
        // The rename is the point: a reader sees the old file or the new one, never half of one.
        assert!(!t.0.join("plugin-state/sidebar.json.tmp").exists());
    }
}
