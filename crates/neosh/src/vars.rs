//! What everybody remembers about something.
//!
//! The other half of [`crate::pstate`], and the difference between them is who is allowed to look.
//! Plugin state is keyed by the calling plugin and is private by construction — right for a panel's
//! fold set, which is nobody else's business. It is wrong for "this project is a favourite", which
//! stopped being one plugin's business the moment a second panel existed: with state, a sidebar of
//! your own starts with no favourites and pinning one in ours is invisible to it.
//!
//! So a var is scoped to the *thing it describes* rather than to whoever wrote it — the workspace, a
//! conversation, a project — and anyone may read or write it. Keys are namespaced by convention
//! (`sidebar.favorite`, `acme.colour`) for the same reason options are: the store cannot stop two
//! plugins choosing the same name, and a prefix is what makes them not want to.
//!
//! One file, `$STATE/vars.json`, because the whole point is that it is a single shared table. Plain
//! JSON on disk, like the rest of `$STATE` — nothing secret belongs here.

use std::collections::BTreeMap;
use std::path::PathBuf;

use neosh_proto::VarScope;
use serde_json::{Map, Value};

/// Every scope that has anything set on it.
///
/// `BTreeMap` rather than `HashMap` so the file is written in a stable order: a store that
/// reshuffles its own keys on every write is one you cannot usefully diff or put in version
/// control, and people do put `$STATE` in version control.
#[derive(Debug, Default)]
pub struct Vars {
    path: Option<PathBuf>,
    scopes: BTreeMap<String, Map<String, Value>>,
    /// Buffer and window scopes: never written to disk, because the things they are about do not
    /// survive the process either.
    transient: BTreeMap<String, Map<String, Value>>,
    loaded: bool,
}

impl Vars {
    pub fn new(state_dir: Option<PathBuf>) -> Self {
        Self {
            path: state_dir.map(|d| d.join("vars.json")),
            scopes: BTreeMap::new(),
            transient: BTreeMap::new(),
            loaded: false,
        }
    }

    fn is_transient(scope: &VarScope) -> bool {
        matches!(scope, VarScope::Buffer { .. } | VarScope::Window { .. })
    }

    fn table(&mut self, scope: &VarScope) -> &mut BTreeMap<String, Map<String, Value>> {
        if Self::is_transient(scope) { &mut self.transient } else { &mut self.scopes }
    }

    pub fn get(&mut self, scope: &VarScope, key: &str) -> Value {
        self.load();
        let id = Self::key(scope);
        self.table(scope).get(&id).and_then(|m| m.get(key)).cloned().unwrap_or(Value::Null)
    }

    /// Everything on one scope, so a panel reads its whole arrangement in one call rather than one
    /// round trip per project.
    pub fn all(&mut self, scope: &VarScope) -> Map<String, Value> {
        self.load();
        let id = Self::key(scope);
        self.table(scope).get(&id).cloned().unwrap_or_default()
    }

    pub fn set(&mut self, scope: &VarScope, key: String, value: Value) {
        self.load();
        let id = Self::key(scope);
        self.table(scope).entry(id).or_default().insert(key, value);
        if !Self::is_transient(scope) {
            self.flush();
        }
    }

    pub fn delete(&mut self, scope: &VarScope, key: &str) {
        self.load();
        let id = Self::key(scope);
        let transient = Self::is_transient(scope);
        let Some(map) = self.table(scope).get_mut(&id) else { return };
        map.remove(key);
        // An empty scope is removed rather than left behind, or a workspace accumulates one entry
        // per conversation anybody ever set a var on and never dropped one.
        if map.is_empty() {
            self.table(scope).remove(&id);
        }
        if !transient {
            self.flush();
        }
    }

    /// Drop a closed window's vars. Returns the keys that went, so their listeners can be told.
    pub fn forget_window(&mut self, win: neosh_proto::WindowId) -> Vec<String> {
        self.transient
            .remove(&Self::key(&VarScope::Window { win }))
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Drop everything about a conversation that has been deleted.
    ///
    /// Called on `session.close`. Without it, deleting a conversation leaves its vars in the file
    /// for ever — and worse, a new conversation could be handed the same id by a future store and
    /// inherit somebody's stale colour.
    pub fn forget_session(&mut self, session: &neosh_proto::SessionId) {
        self.load();
        let id = Self::key(&VarScope::Session { session: session.clone() });
        if self.scopes.remove(&id).is_some() {
            self.flush();
        }
    }

    /// The on-disk name of a scope. Prefixed by kind so a project whose path happens to look like a
    /// conversation id cannot collide with one.
    fn key(scope: &VarScope) -> String {
        match scope {
            VarScope::Global => "global".to_string(),
            VarScope::Session { session } => format!("session:{session}"),
            VarScope::Project { cwd } => format!("project:{cwd}"),
            VarScope::Buffer { buf } => format!("buf:{}", buf.0),
            VarScope::Window { win } => format!("win:{}", win.0),
        }
    }

    fn load(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        let Some(path) = self.path.as_ref() else { return };
        let Ok(bytes) = std::fs::read(path) else { return };
        let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&bytes) else {
            // Valid JSON that is not an object, or not JSON at all: treat as empty rather than
            // refusing to start. Losing somebody's pins is bad; refusing to open their workspace
            // over them is worse.
            tracing::warn!(?path, "vars file is not a JSON object; starting empty");
            return;
        };
        for (scope, value) in map {
            if let Value::Object(inner) = value {
                self.scopes.insert(scope, inner);
            }
        }
    }

    /// Written to a temporary file and renamed, so a crash mid-write leaves yesterday's values
    /// rather than half a file that fails to parse and silently loses all of them.
    fn flush(&mut self) {
        let Some(path) = self.path.as_ref() else { return };
        let write = || -> std::io::Result<()> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let json = serde_json::to_vec_pretty(&self.scopes)
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            tracing::warn!("could not save shared vars: {e}");
        }
    }
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
        let p = std::env::temp_dir().join(format!("neosh-vars-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("tmp");
        Tmp(p)
    }

    fn project(cwd: &str) -> VarScope {
        VarScope::Project { cwd: cwd.to_string() }
    }

    #[test]
    fn a_project_var_survives_a_restart() {
        let t = tmp("restart");
        let mut v = Vars::new(Some(t.0.clone()));
        v.set(&project("/w/neosh"), "sidebar.favorite".into(), json!(true));

        let mut fresh = Vars::new(Some(t.0.clone()));
        assert_eq!(fresh.get(&project("/w/neosh"), "sidebar.favorite"), json!(true));
    }

    /// The whole difference from plugin state: whoever wrote it, anyone can read it. There is no
    /// plugin argument on any of these calls, which is the API making that guarantee structural.
    #[test]
    fn a_var_is_not_scoped_to_whoever_wrote_it() {
        let mut v = Vars::new(None);
        v.set(&project("/w/a"), "colour".into(), json!("blue"));
        assert_eq!(v.get(&project("/w/a"), "colour"), json!("blue"));
    }

    #[test]
    fn scopes_of_different_kinds_do_not_collide() {
        let mut v = Vars::new(None);
        v.set(&VarScope::Global, "k".into(), json!(1));
        v.set(&project("/w/a"), "k".into(), json!(2));
        assert_eq!(v.get(&VarScope::Global, "k"), json!(1));
        assert_eq!(v.get(&project("/w/a"), "k"), json!(2));
    }

    #[test]
    fn all_answers_one_scope_whole() {
        let mut v = Vars::new(None);
        v.set(&project("/w/a"), "one".into(), json!(1));
        v.set(&project("/w/a"), "two".into(), json!(2));
        v.set(&project("/w/b"), "three".into(), json!(3));
        let all = v.all(&project("/w/a"));
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("one"), Some(&json!(1)));
    }

    #[test]
    fn deleting_the_last_key_takes_the_scope_with_it() {
        let t = tmp("empty");
        let mut v = Vars::new(Some(t.0.clone()));
        v.set(&project("/w/a"), "k".into(), json!(1));
        v.delete(&project("/w/a"), "k");
        assert!(v.scopes.is_empty(), "an empty scope should not be kept");
    }

    #[test]
    fn a_deleted_conversation_takes_its_vars_with_it() {
        let mut v = Vars::new(None);
        let s = neosh_proto::SessionId("abc".into());
        v.set(&VarScope::Session { session: s.clone() }, "k".into(), json!(1));
        v.forget_session(&s);
        assert_eq!(v.get(&VarScope::Session { session: s }, "k"), Value::Null);
    }

    #[test]
    fn a_corrupt_file_reads_as_empty_rather_than_failing() {
        let t = tmp("corrupt");
        std::fs::write(t.0.join("vars.json"), b"{not json").expect("write");
        let mut v = Vars::new(Some(t.0.clone()));
        assert_eq!(v.get(&VarScope::Global, "anything"), Value::Null);
        v.set(&VarScope::Global, "k".into(), json!(2));
        assert_eq!(v.get(&VarScope::Global, "k"), json!(2));
    }

    #[test]
    fn writes_are_atomic() {
        let t = tmp("atomic");
        let mut v = Vars::new(Some(t.0.clone()));
        v.set(&VarScope::Global, "k".into(), json!("v"));
        // The rename is the point: a reader sees the old file or the new one, never half of one.
        assert!(!t.0.join("vars.json.tmp").exists());
    }
}
