//! Conversations on disk.
//!
//! One JSON file per session under `$STATE/sessions/`, plus an `order` file recording which one you
//! were in last and in what order you used them. One file per session rather than one big file
//! because the failure modes differ: a truncated write loses one conversation instead of all of
//! them, and appending to a session does not rewrite the others.
//!
//! **Nothing secret is written here.** A session holds what you typed and what the model said; keys
//! live in the environment and are resolved per request. `InstanceConfig` — which names an
//! `AuthRef`, never a value — is deliberately not part of what is saved.

use std::path::{Path, PathBuf};

use neosh_agent::{Session, store::SessionStore};
use neosh_proto::{Message, ModelSelection, SessionId, Usage};
use serde::{Deserialize, Serialize};

/// How many conversations to restore.
///
/// A cap rather than a sweep: deleting someone's history because they have a lot of it would be a
/// spectacular thing for an editor to do on startup. The rest stay on disk and are simply not
/// loaded.
const RESTORE_LIMIT: usize = 200;

#[derive(Serialize, Deserialize)]
struct Stored {
    id: SessionId,
    cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selection: Option<ModelSelection>,
    #[serde(default)]
    usage: Usage,
    /// How full the window was on the last request. Persisted so a reopened conversation can say
    /// how much room is left without having to send something first.
    #[serde(default)]
    context_tokens: u64,
    /// And the window it was a fraction of, when a driver said. Persisted with it for the same
    /// reason: a reopened conversation should read the same as it did before it was closed, and a
    /// meter whose denominator changed on reload would move without anything having happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<u64>,
    /// Put away rather than deleted. Defaulted, so a session written before archiving existed
    /// comes back in the open list, which is where it was.
    #[serde(default)]
    archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archived_at: Option<i64>,
    /// What the agent may do without asking here. Absent for a conversation that has never been
    /// given one, which then takes whatever `config.toml` says — so raising or lowering the default
    /// still reaches every conversation that never overrode it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permission_mode: Option<neosh_proto::PermissionMode>,
    /// The system prompt is *not* stored. It comes from configuration, and a stale copy pinned into
    /// a saved session would silently outrank the one the user edited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    _reserved: Option<()>,
}

#[derive(Serialize, Deserialize, Default)]
struct Order {
    #[serde(default)]
    order: Vec<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<SessionId>,
}

fn dir(state: &Path) -> PathBuf {
    state.join("sessions")
}

fn order_path(state: &Path) -> PathBuf {
    dir(state).join("order.json")
}

fn session_path(state: &Path, id: &SessionId) -> PathBuf {
    // Session ids are generated, not user input, but a path is a path: anything that is not a plain
    // id is refused rather than joined.
    dir(state).join(format!("{}.json", id.0))
}

fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl From<&Session> for Stored {
    fn from(s: &Session) -> Self {
        Self {
            id: s.id.clone(),
            cwd: s.cwd.display().to_string(),
            title: s.title.clone(),
            created_at: s.created_at,
            updated_at: s.updated_at,
            messages: s.messages.clone(),
            selection: s.selection.clone(),
            usage: s.usage,
            context_tokens: s.context_tokens,
            context_window: s.context_window,
            archived: s.archived,
            archived_at: s.archived_at,
            permission_mode: s.permission_mode,
            _reserved: None,
        }
    }
}

impl Stored {
    fn into_session(self) -> Session {
        let mut s = Session::new(self.cwd);
        s.id = self.id;
        s.title = self.title;
        s.created_at = self.created_at;
        s.updated_at = self.updated_at;
        s.messages = self.messages;
        s.selection = self.selection;
        s.usage = self.usage;
        s.context_tokens = self.context_tokens;
        s.context_window = self.context_window;
        s.archived = self.archived;
        s.archived_at = self.archived_at;
        s.permission_mode = self.permission_mode;
        s
    }
}

/// Write one session.
///
/// Written to a temporary file and renamed, so a crash mid-write leaves the previous version rather
/// than half of the new one. A conversation you can no longer open is worse than one that is a
/// minute out of date.
pub fn save(state: &Path, session: &Session) -> std::io::Result<()> {
    if !is_safe_id(&session.id.0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing to write a session id that is not a plain identifier: {:?}", session.id.0),
        ));
    }
    std::fs::create_dir_all(dir(state))?;
    let path = session_path(state, &session.id);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(&Stored::from(session))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

/// Write every session plus the ordering.
pub fn save_all(state: &Path, store: &SessionStore) -> std::io::Result<()> {
    std::fs::create_dir_all(dir(state))?;
    for s in store.iter() {
        save(state, s)?;
    }
    let order = Order {
        order: store.order().to_vec(),
        active: Some(store.active_id().clone()),
    };
    let path = order_path(state);
    let tmp = path.with_extension("json.tmp");
    let json =
        serde_json::to_vec_pretty(&order).map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)
}

pub fn forget(state: &Path, id: &SessionId) {
    if is_safe_id(&id.0) {
        let _ = std::fs::remove_file(session_path(state, id));
    }
}

/// Everything restorable, most recently used first, and which one was active.
///
/// A file that does not parse is skipped with a warning rather than being fatal: one corrupt
/// conversation must not stop neosh from starting, and the remaining ones are still yours.
pub fn load(state: &Path) -> (Vec<Session>, Option<SessionId>) {
    let d = dir(state);
    let Ok(entries) = std::fs::read_dir(&d) else {
        return (Vec::new(), None);
    };

    let order: Order = std::fs::read_to_string(order_path(state))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();

    let mut sessions: Vec<Session> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("order.json") {
            continue;
        }
        match std::fs::read_to_string(&path).ok().map(|t| serde_json::from_str::<Stored>(&t)) {
            Some(Ok(stored)) => sessions.push(stored.into_session()),
            Some(Err(e)) => tracing::warn!("{}: not a readable session ({e})", path.display()),
            None => {}
        }
    }

    // Ordering: whatever the order file says first, then anything it did not mention, newest first.
    let rank = |id: &SessionId| order.order.iter().position(|x| x == id).unwrap_or(usize::MAX);
    sessions.sort_by(|a, b| {
        rank(&a.id).cmp(&rank(&b.id)).then(b.updated_at.cmp(&a.updated_at))
    });
    if sessions.len() > RESTORE_LIMIT {
        tracing::info!(
            "{} saved conversations; restoring the {RESTORE_LIMIT} most recent",
            sessions.len()
        );
        sessions.truncate(RESTORE_LIMIT);
    }

    // An archived conversation is never the one you land in: it is the one you deliberately put
    // away, and starting there would make archiving feel like it had not worked.
    let active = order
        .active
        .filter(|id| sessions.iter().any(|s| &s.id == id && !s.archived));
    (sessions, active)
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
        let p = std::env::temp_dir().join(format!("neosh-sessions-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("tmp");
        Tmp(p)
    }

    fn session(text: &str) -> Session {
        let mut s = Session::new("/work");
        s.push_user_text(text);
        s.created_at = 100;
        s.updated_at = 100;
        s
    }

    #[test]
    fn a_saved_conversation_comes_back() {
        let t = tmp("roundtrip");
        let mut original = session("what is 2+2?");
        original.title = Some("Arithmetic".into());
        original.usage = Usage { input_tokens: 7, output_tokens: 3, ..Default::default() };
        save(&t.0, &original).expect("save");

        let (loaded, _) = load(&t.0);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, original.id);
        assert_eq!(loaded[0].title.as_deref(), Some("Arithmetic"));
        assert_eq!(loaded[0].messages.len(), 1);
        assert_eq!(loaded[0].usage.input_tokens, 7);
        assert_eq!(loaded[0].cwd, original.cwd);
    }

    /// A mode you chose for one conversation is a decision, not a mood.
    ///
    /// It survives the workspace stopping, and a conversation that never had one comes back
    /// without one — so raising or lowering `permissions.mode` in `config.toml` still reaches every
    /// conversation that never overrode it, rather than being frozen into each one at creation.
    #[test]
    fn a_conversations_permission_mode_is_saved_and_the_absence_of_one_is_too() {
        let t = tmp("permmode");
        let mut locked = session("careful now");
        locked.permission_mode = Some(neosh_proto::PermissionMode::Deny);
        let ordinary = session("anything at all");
        save(&t.0, &locked).expect("save");
        save(&t.0, &ordinary).expect("save");

        let (loaded, _) = load(&t.0);
        let of = |id: &neosh_proto::SessionId| {
            loaded.iter().find(|s| &s.id == id).expect("saved").permission_mode
        };
        assert_eq!(of(&locked.id), Some(neosh_proto::PermissionMode::Deny));
        assert_eq!(of(&ordinary.id), None, "and none is not a mode");
    }

    #[test]
    fn nothing_saved_is_not_an_error() {
        let t = tmp("empty");
        let (loaded, active) = load(&t.0);
        assert!(loaded.is_empty());
        assert_eq!(active, None);
    }

    #[test]
    fn a_corrupt_file_is_skipped_rather_than_fatal() {
        // One unreadable conversation must not stop neosh from starting; the rest are still yours.
        let t = tmp("corrupt");
        save(&t.0, &session("good")).expect("save");
        std::fs::write(dir(&t.0).join("broken.json"), "{ not json").expect("write");

        let (loaded, _) = load(&t.0);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].messages.len(), 1);
    }

    #[test]
    fn the_order_file_decides_the_order_and_the_active_session() {
        let t = tmp("order");
        let a = session("first");
        let b = session("second");
        let (ia, ib) = (a.id.clone(), b.id.clone());
        let mut store = SessionStore::new(a);
        store.insert(b);
        store.switch(&ib).expect("switch");
        save_all(&t.0, &store).expect("save");

        let (loaded, active) = load(&t.0);
        assert_eq!(active.as_ref(), Some(&ib), "the one you were in");
        assert_eq!(loaded[0].id, ib, "most recently used first");
        assert_eq!(loaded[1].id, ia);
    }

    #[test]
    fn an_active_id_pointing_at_a_deleted_session_is_dropped() {
        let t = tmp("stale-active");
        let a = session("only");
        let ia = a.id.clone();
        let store = SessionStore::new(a);
        save_all(&t.0, &store).expect("save");
        forget(&t.0, &ia);

        let (loaded, active) = load(&t.0);
        assert!(loaded.is_empty());
        assert_eq!(active, None, "restoring would otherwise switch to a session that is not there");
    }

    #[test]
    fn forget_removes_the_file() {
        let t = tmp("forget");
        let s = session("gone");
        save(&t.0, &s).expect("save");
        assert!(session_path(&t.0, &s.id).exists());
        forget(&t.0, &s.id);
        assert!(!session_path(&t.0, &s.id).exists());
        assert!(load(&t.0).0.is_empty());
    }

    #[test]
    fn a_session_id_that_is_not_a_plain_identifier_is_refused() {
        // Ids are generated rather than typed, but a path built from data is still a path.
        let t = tmp("traversal");
        let mut s = session("x");
        s.id = SessionId("../../escape".into());
        assert!(save(&t.0, &s).is_err());
        assert!(!t.0.parent().map(|p| p.join("escape.json").exists()).unwrap_or(false));
    }

    #[test]
    fn the_system_prompt_is_not_persisted() {
        // It comes from configuration; a copy pinned into a saved session would silently outrank
        // the one the user edited.
        let t = tmp("system");
        let mut s = session("x");
        s.system = Some("You are a pirate.".into());
        save(&t.0, &s).expect("save");
        let text = std::fs::read_to_string(session_path(&t.0, &s.id)).expect("read");
        assert!(!text.contains("pirate"), "found it in {text}");
        assert_eq!(load(&t.0).0[0].system, None);
    }

    #[test]
    fn a_partial_write_cannot_replace_a_good_file() {
        // The rename is the point: readers see either the old file or the new one, never half.
        let t = tmp("atomic");
        let s = session("original");
        save(&t.0, &s).expect("save");
        let leftover = session_path(&t.0, &s.id).with_extension("json.tmp");
        assert!(!leftover.exists(), "the temporary file is renamed, not left behind");
    }
}
