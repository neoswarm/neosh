//! Many conversations, one of them active.
//!
//! A workspace is not one conversation. You have a branch you are working on, a review you are
//! half-way through, and a question you asked yesterday that you want to reread — and switching
//! between them must not mean losing any of them.
//!
//! This type is pure data: no clock, no filesystem, no ordering by wall time it cannot verify.
//! Timestamps arrive from the caller, so the store is deterministic and testable, and persistence
//! lives in the binary where the state directory is already known.

use std::collections::BTreeMap;

use neosh_proto::{SessionId, SessionInfo};

use crate::session::Session;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum StoreError {
    #[error("no session {0}")]
    NoSuchSession(SessionId),
    #[error("the last session cannot be closed")]
    LastSession,
}

/// Every conversation this process knows about.
#[derive(Debug)]
pub struct SessionStore {
    active: Session,
    idle: BTreeMap<SessionId, Session>,
    /// Most recently active first. Not derived from timestamps: two sessions created in the same
    /// millisecond would order arbitrarily, and a list that reshuffles itself is a list nobody can
    /// point at.
    order: Vec<SessionId>,
}

impl SessionStore {
    pub fn new(session: Session) -> Self {
        let order = vec![session.id.clone()];
        Self { active: session, idle: BTreeMap::new(), order }
    }

    pub fn active(&self) -> &Session {
        &self.active
    }

    pub fn active_mut(&mut self) -> &mut Session {
        &mut self.active
    }

    pub fn active_id(&self) -> &SessionId {
        &self.active.id
    }

    pub fn len(&self) -> usize {
        self.idle.len() + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn get(&self, id: &SessionId) -> Option<&Session> {
        if &self.active.id == id { Some(&self.active) } else { self.idle.get(id) }
    }

    /// Add a session without switching to it.
    ///
    /// Separate from `switch` because restoring a saved workspace inserts many sessions and must
    /// not make each one active in turn.
    pub fn insert(&mut self, session: Session) {
        let id = session.id.clone();
        if id == self.active.id {
            self.active = session;
            return;
        }
        self.idle.insert(id.clone(), session);
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    /// Make `id` the active session, parking the current one.
    pub fn switch(&mut self, id: &SessionId) -> Result<(), StoreError> {
        if &self.active.id == id {
            return Ok(());
        }
        let next = self.idle.remove(id).ok_or_else(|| StoreError::NoSuchSession(id.clone()))?;
        let previous = std::mem::replace(&mut self.active, next);
        self.idle.insert(previous.id.clone(), previous);
        // Move to the front rather than re-sorting: "most recently active" is a history, and a
        // history is something you append to.
        self.order.retain(|x| x != id);
        self.order.insert(0, id.clone());
        Ok(())
    }

    /// Close a session. The active one may be closed as long as another exists to fall back to.
    pub fn remove(&mut self, id: &SessionId) -> Result<(), StoreError> {
        if &self.active.id != id {
            self.idle.remove(id).ok_or_else(|| StoreError::NoSuchSession(id.clone()))?;
            self.order.retain(|x| x != id);
            return Ok(());
        }
        // Closing what you are looking at has to land you somewhere, and the most recently used
        // other session is the least surprising place.
        let next = self
            .order
            .iter()
            .find(|candidate| *candidate != id && self.idle.contains_key(*candidate))
            .cloned()
            .or_else(|| self.idle.keys().next().cloned())
            .ok_or(StoreError::LastSession)?;
        let replacement = self.idle.remove(&next).ok_or(StoreError::LastSession)?;
        self.active = replacement;
        self.order.retain(|x| x != id);
        self.order.retain(|x| x != &next);
        self.order.insert(0, next);
        Ok(())
    }

    pub fn rename(&mut self, id: &SessionId, title: Option<String>) -> Result<(), StoreError> {
        let s = if &self.active.id == id {
            &mut self.active
        } else {
            self.idle.get_mut(id).ok_or_else(|| StoreError::NoSuchSession(id.clone()))?
        };
        s.title = title.filter(|t| !t.trim().is_empty());
        Ok(())
    }

    /// Every session, most recently active first, with the active one flagged.
    pub fn list(&self) -> Vec<SessionInfo> {
        let mut seen: Vec<SessionInfo> = Vec::with_capacity(self.len());
        for id in &self.order {
            if let Some(s) = self.get(id) {
                seen.push(self.info(s));
            }
        }
        // Anything inserted without touching `order` still has to appear; a session you cannot see
        // is a session you cannot get back to.
        for (id, s) in &self.idle {
            if !self.order.contains(id) {
                seen.push(self.info(s));
            }
        }
        if !self.order.contains(&self.active.id) {
            seen.insert(0, self.info(&self.active));
        }
        seen
    }

    /// Every session, for persistence.
    pub fn iter(&self) -> impl Iterator<Item = &Session> {
        std::iter::once(&self.active).chain(self.idle.values())
    }

    fn info(&self, s: &Session) -> SessionInfo {
        let mut info = s.info();
        info.is_active = s.id == self.active.id;
        info
    }

    /// Restore the most-recently-active ordering from a saved workspace.
    pub fn set_order(&mut self, order: Vec<SessionId>) {
        self.order = order;
    }

    pub fn order(&self) -> &[SessionId] {
        &self.order
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (SessionStore, SessionId, SessionId, SessionId) {
        let a = Session::new("/a");
        let b = Session::new("/b");
        let c = Session::new("/c");
        let (ia, ib, ic) = (a.id.clone(), b.id.clone(), c.id.clone());
        let mut s = SessionStore::new(a);
        s.insert(b);
        s.insert(c);
        (s, ia, ib, ic)
    }

    #[test]
    fn a_new_store_has_exactly_one_session_and_it_is_active() {
        let s = SessionStore::new(Session::new("/x"));
        assert_eq!(s.len(), 1);
        assert_eq!(s.list().len(), 1);
        assert!(s.list()[0].is_active);
    }

    #[test]
    fn switching_parks_the_previous_session_without_losing_it() {
        let (mut s, a, b, _c) = store();
        s.active_mut().push_user_text("first");
        s.switch(&b).expect("switch");
        assert_eq!(s.active_id(), &b);
        assert!(s.active().messages.is_empty(), "the new session starts where it was");

        s.switch(&a).expect("switch back");
        assert_eq!(s.active().messages.len(), 1, "the parked conversation survived");
    }

    #[test]
    fn switching_to_the_active_session_is_a_no_op_not_an_error() {
        let (mut s, a, _b, _c) = store();
        assert_eq!(s.switch(&a), Ok(()));
        assert_eq!(s.active_id(), &a);
    }

    #[test]
    fn switching_to_something_that_does_not_exist_is_an_error() {
        let (mut s, _a, _b, _c) = store();
        let ghost = Session::new("/ghost").id;
        assert_eq!(s.switch(&ghost), Err(StoreError::NoSuchSession(ghost)));
    }

    #[test]
    fn the_list_is_most_recently_active_first() {
        let (mut s, a, b, c) = store();
        s.switch(&c).unwrap();
        s.switch(&b).unwrap();
        let ids: Vec<_> = s.list().into_iter().map(|i| i.id).collect();
        assert_eq!(ids[0], b, "the one you are in comes first");
        assert_eq!(ids[1], c, "then the one before it");
        assert!(ids.contains(&a));
        assert_eq!(s.list().iter().filter(|i| i.is_active).count(), 1);
    }

    #[test]
    fn every_session_appears_in_the_list_even_if_never_switched_to() {
        // A session you cannot see is a session you cannot get back to.
        let (s, a, b, c) = store();
        let ids: Vec<_> = s.list().into_iter().map(|i| i.id).collect();
        for want in [a, b, c] {
            assert!(ids.contains(&want), "{want:?} missing from {ids:?}");
        }
    }

    #[test]
    fn closing_the_active_session_lands_on_the_most_recent_other_one() {
        let (mut s, a, b, c) = store();
        s.switch(&c).unwrap();
        s.switch(&b).unwrap();
        s.remove(&b).expect("close");
        assert_eq!(s.active_id(), &c, "the one you were in before, not an arbitrary one");
        assert_eq!(s.len(), 2);
        assert!(s.get(&b).is_none());
        assert!(s.get(&a).is_some());
    }

    #[test]
    fn closing_an_idle_session_leaves_the_active_one_alone() {
        let (mut s, a, b, _c) = store();
        s.remove(&b).expect("close");
        assert_eq!(s.active_id(), &a);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn the_last_session_cannot_be_closed() {
        // There is always a conversation. A neosh with none has nowhere to put what you type next.
        let mut s = SessionStore::new(Session::new("/only"));
        let only = s.active_id().clone();
        assert_eq!(s.remove(&only), Err(StoreError::LastSession));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn renaming_trims_and_treats_blank_as_no_title() {
        let (mut s, a, _b, _c) = store();
        s.rename(&a, Some("  Fix login  ".into())).unwrap();
        assert_eq!(s.get(&a).unwrap().title.as_deref(), Some("  Fix login  "));
        s.rename(&a, Some("   ".into())).unwrap();
        assert_eq!(s.get(&a).unwrap().title, None, "a blank title is no title, not a blank one");
    }

    #[test]
    fn inserting_a_session_that_is_already_active_replaces_it() {
        let mut s = SessionStore::new(Session::new("/x"));
        let id = s.active_id().clone();
        let mut replacement = Session::new("/x");
        replacement.id = id.clone();
        replacement.push_user_text("restored");
        s.insert(replacement);
        assert_eq!(s.len(), 1, "not duplicated into idle");
        assert_eq!(s.active().messages.len(), 1);
    }

    #[test]
    fn iter_yields_every_session_exactly_once() {
        let (s, _a, _b, _c) = store();
        let ids: Vec<_> = s.iter().map(|x| x.id.clone()).collect();
        assert_eq!(ids.len(), 3);
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 3);
    }
}
