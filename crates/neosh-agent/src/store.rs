//! Many conversations, and however many terminals looking at them.
//!
//! A workspace is not one conversation. You have a branch you are working on, a review you are
//! half-way through, and a question you asked yesterday that you want to reread — and switching
//! between them must not mean losing any of them.
//!
//! **None of them is "the active one", and this type used to say otherwise.** It held one session
//! out of the map and called it active, which was a fair description of a workspace with one
//! screen and is a false one the moment there are two: a terminal in one conversation beside a
//! terminal in another has two answers, and neither of them belongs to the store. What is here is
//! every conversation and the order they were last arrived in; *who is looking at what* is the
//! host's, one answer per view.
//!
//! What survives of the old idea is [`SessionStore::current`], which is the most recently arrived
//! in anywhere. That is a real thing to want — a question asked outside any turn is about
//! somewhere, and a workspace with nothing attached still has a most recent conversation — but it
//! is a fallback and never a place somebody is.
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
    sessions: BTreeMap<SessionId, Session>,
    /// Most recently arrived in first. Not derived from timestamps: two sessions created in the
    /// same millisecond would order arbitrarily, and a list that reshuffles itself is a list
    /// nobody can point at.
    ///
    /// Every session is in here. It used to hold only the parked ones, with the active one kept
    /// beside it, and the two had to be reconciled in four places.
    order: Vec<SessionId>,
}

impl SessionStore {
    pub fn new(session: Session) -> Self {
        let order = vec![session.id.clone()];
        let mut sessions = BTreeMap::new();
        sessions.insert(session.id.clone(), session);
        Self { sessions, order }
    }

    /// The conversation most recently arrived in, anywhere in this workspace.
    ///
    /// Not "the one on screen": there may be several screens, and there may be none. This is what
    /// answers a question that has to be about *some* conversation and has no view to ask —
    /// a plugin's `neosh.permit` outside a turn, a workspace deciding where a fresh terminal
    /// should land. A caller that has a view has a better answer and should use it.
    pub fn current(&self) -> &Session {
        self.sessions
            .get(self.current_id())
            .expect("the order names a session that is in the store")
    }

    pub fn current_mut(&mut self) -> &mut Session {
        let id = self.current_id().clone();
        self.sessions.get_mut(&id).expect("the order names a session that is in the store")
    }

    pub fn current_id(&self) -> &SessionId {
        self.order
            .first()
            .or_else(|| self.sessions.keys().next())
            .expect("a store always holds at least one conversation")
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn get(&self, id: &SessionId) -> Option<&Session> {
        self.sessions.get(id)
    }

    /// Public because a turn reaches its own conversation by id: it may not be the one anybody is
    /// looking at, and by the time the turn ends it may not even be open.
    pub fn get_mut(&mut self, id: &SessionId) -> Option<&mut Session> {
        self.sessions.get_mut(id)
    }

    /// Add a session without arriving in it.
    ///
    /// Separate from [`enter`](Self::enter) because restoring a saved workspace inserts many
    /// sessions and must not make each one the most recent in turn.
    pub fn insert(&mut self, session: Session) {
        let id = session.id.clone();
        self.sessions.insert(id.clone(), session);
        if !self.order.contains(&id) {
            self.order.push(id);
        }
    }

    /// Arrive in a conversation: it becomes the most recent, and it has been read.
    ///
    /// Nothing is parked, because nothing was ever taken out to be parked. What *is* dropped is a
    /// placeholder you were in and have now left: it exists only because the workspace had to have
    /// somewhere to type, and the moment you are somewhere else it is an empty conversation nobody
    /// asked for, invisible in every list, that a later `step away` could otherwise land you in
    /// instead of something real.
    ///
    /// `leaving` is the conversation this view was in, or `None` for a view arriving from nowhere
    /// — a terminal attaching. Passed in rather than read off the store, because with several
    /// terminals the store has no idea which one moved.
    pub fn enter(&mut self, id: &SessionId, leaving: Option<&SessionId>) -> Result<(), StoreError> {
        if !self.sessions.contains_key(id) {
            return Err(StoreError::NoSuchSession(id.clone()));
        }
        // Arriving is what reading is. Cleared here rather than by whoever draws the list, so that
        // every list agrees the moment you switch — a panel that cleared its own copy would keep
        // saying "new" in the status line you did not happen to be looking at.
        if let Some(s) = self.sessions.get_mut(id) {
            s.unread = false;
        }
        if let Some(left) = leaving.filter(|l| *l != id) {
            let drop = self.sessions.get(left).is_some_and(|s| s.ephemeral);
            if drop {
                self.sessions.remove(left);
                self.order.retain(|x| x != left);
            }
        }
        // Move to the front rather than re-sorting: "most recently arrived in" is a history, and a
        // history is something you append to.
        self.order.retain(|x| x != id);
        self.order.insert(0, id.clone());
        Ok(())
    }

    /// Close a session.
    ///
    /// Whether anybody was looking at it is not the store's business — the host moves its views
    /// somewhere else first, which is what [`next_after`](Self::next_after) is for.
    pub fn remove(&mut self, id: &SessionId) -> Result<(), StoreError> {
        if self.sessions.len() == 1 && self.sessions.contains_key(id) {
            return Err(StoreError::LastSession);
        }
        self.sessions.remove(id).ok_or_else(|| StoreError::NoSuchSession(id.clone()))?;
        self.order.retain(|x| x != id);
        Ok(())
    }

    /// Put a conversation away, or bring it back.
    ///
    /// Archiving is *not* closing: the session stays in the store, keeps its messages and comes
    /// back the moment you unarchive it. Only its place in [`list`](Self::list) changes.
    ///
    /// A conversation somebody is looking at cannot be left archived — you would be typing into
    /// something the list no longer shows — but moving them is the host's job, since only it knows
    /// who is where. What the store refuses is archiving the *last* open one, because then there
    /// is nowhere for anybody to go. `at` is the clock the caller read; this type does not read
    /// one.
    pub fn archive(&mut self, id: &SessionId, archived: bool, at: i64) -> Result<(), StoreError> {
        if !self.sessions.contains_key(id) {
            return Err(StoreError::NoSuchSession(id.clone()));
        }
        if archived && self.next_after(id).is_none() {
            return Err(StoreError::LastSession);
        }
        let s = self.get_mut(id).ok_or_else(|| StoreError::NoSuchSession(id.clone()))?;
        s.archived = archived;
        s.archived_at = archived.then_some(at);
        if !archived {
            // Coming back puts it at the top of the list, because you unarchived it in order to
            // look at it.
            self.order.retain(|x| x != id);
            self.order.insert(0, id.clone());
        }
        Ok(())
    }

    /// You are looking at it, so whatever finished while you were away has been seen.
    ///
    /// Answers whether anything changed, because the caller's next move is a write to disk and
    /// re-persisting every conversation on every arrival is a cost for nothing.
    pub fn mark_read(&mut self, id: &SessionId) -> bool {
        match self.get_mut(id) {
            Some(s) if s.unread => {
                s.unread = false;
                true
            }
            _ => false,
        }
    }

    /// Where to go when leaving `id` — the most recently used other conversation.
    ///
    /// Archived ones are skipped: landing in a conversation you deliberately put away is the one
    /// destination that is definitely wrong. `None` when there is nowhere to go, which the host
    /// answers by starting an empty conversation — the store cannot, because it can read neither a
    /// clock nor a working directory.
    ///
    /// A *query* rather than a move, because with several terminals there is no single "you" to
    /// take somewhere. Two views on the conversation being closed each ask this and each go.
    /// A placeholder is a last resort rather than excluded: it is somewhere to type, which is the
    /// whole of what it is for, and refusing to land there would mean a workspace you have cleared
    /// out cannot archive its last real conversation.
    pub fn next_after(&self, id: &SessionId) -> Option<SessionId> {
        let usable = |c: &&SessionId| {
            *c != id && self.sessions.get(*c).is_some_and(|s| !s.archived)
        };
        let real = |c: &&SessionId| usable(c) && self.sessions.get(*c).is_some_and(|s| !s.ephemeral);
        self.order
            .iter()
            .find(real)
            .or_else(|| self.sessions.keys().find(real))
            .or_else(|| self.order.iter().find(usable))
            .or_else(|| self.sessions.keys().find(usable))
            .cloned()
    }

    pub fn rename(&mut self, id: &SessionId, title: Option<String>) -> Result<(), StoreError> {
        let s = self.get_mut(id).ok_or_else(|| StoreError::NoSuchSession(id.clone()))?;
        s.title = title.filter(|t| !t.trim().is_empty());
        Ok(())
    }

    /// The conversations you work in, most recently arrived in first.
    ///
    /// Archived ones are left out. That is the whole point of archiving, and making every caller
    /// remember to filter would mean the one that forgot is the one that ships.
    pub fn list(&self) -> Vec<SessionInfo> {
        self.list_with(false)
    }

    /// Every session, most recently arrived in first.
    ///
    /// A placeholder is never in it, archived or not: it is somewhere to type rather than
    /// something you started, and a workspace you have just cleared out should read as empty.
    /// See [`Session::ephemeral`](crate::session::Session::ephemeral).
    ///
    /// `is_active` is the most recent one, which is a guess and is meant to be overwritten. Only
    /// the host knows which conversation is on screen in the terminal that asked, and it stamps
    /// that in before the list reaches anybody who draws it.
    pub fn list_with(&self, include_archived: bool) -> Vec<SessionInfo> {
        let current = self.current_id().clone();
        let mut seen: Vec<SessionInfo> = Vec::with_capacity(self.len());
        for id in &self.order {
            if let Some(s) = self.get(id).filter(|s| !s.ephemeral) {
                let mut info = s.info();
                info.is_active = s.id == current;
                seen.push(info);
            }
        }
        // Anything inserted without touching `order` still has to appear; a session you cannot see
        // is a session you cannot get back to.
        for (id, s) in &self.sessions {
            if !self.order.contains(id) && !s.ephemeral {
                let mut info = s.info();
                info.is_active = s.id == current;
                seen.push(info);
            }
        }
        if !include_archived {
            seen.retain(|s| !s.archived);
        }
        seen
    }

    /// Whether there is a conversation you could go to, as opposed to somewhere to start one.
    ///
    /// The same question [`list`](Self::list) answers, asked without building the list. Archived
    /// ones do not count: what you have put away is not in any list you work from, so a workspace
    /// with nothing but an archive in it reads as empty — and the way back to that archive is one
    /// of the things an empty workspace has to say. A placeholder does not count either, which is
    /// the whole of what a placeholder is.
    pub fn any_open(&self) -> bool {
        self.sessions.values().any(|s| !s.ephemeral && !s.archived)
    }

    /// Every session, for persistence.
    pub fn iter(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
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

    /// Arrive somewhere, the way one terminal does.
    fn go(s: &mut SessionStore, from: &SessionId, to: &SessionId) {
        s.enter(to, Some(from)).expect("enter");
    }

    #[test]
    fn archiving_hides_a_conversation_without_losing_it() {
        let (mut s, _a, b, _c) = store();
        s.archive(&b, true, 1234).expect("archives");
        assert!(!s.list().iter().any(|i| i.id == b), "still in the everyday list");
        assert!(s.list_with(true).iter().any(|i| i.id == b), "gone from the store entirely");
        let kept = s.get(&b).expect("the session itself survives");
        assert!(kept.archived);
        assert_eq!(kept.archived_at, Some(1234));
    }

    #[test]
    fn archiving_says_where_a_terminal_in_it_should_go() {
        // The store no longer moves anybody: with several terminals there is no single "you" to
        // take somewhere, and two views on the conversation being put away each have to be told.
        let (mut s, a, _b, _c) = store();
        let next = s.next_after(&a).expect("somewhere to go");
        s.archive(&a, true, 1).expect("archives");
        assert_ne!(next, a);
        assert!(!s.get(&next).expect("there").archived);
        assert!(s.list().iter().all(|i| i.id != a));
    }

    #[test]
    fn a_placeholder_is_in_no_list_and_is_dropped_the_moment_you_are_somewhere_else() {
        // Somewhere to type after you have cleared the workspace out, and nothing more than that.
        // In a list it would be a conversation you never started, in a project you had just
        // emptied; parked on a switch it would be one more of them every time.
        let (mut s, _a, b, _c) = store();
        let here = s.current_id().clone();
        s.get_mut(&here).expect("there").ephemeral = true;
        assert!(s.list().iter().all(|i| i.id != here), "not in the everyday list");
        assert!(s.list_with(true).iter().all(|i| i.id != here), "nor in the one with the archive");
        assert!(s.any_open(), "the real conversations are still there");

        go(&mut s, &here, &b);
        assert!(s.get(&here).is_none(), "gone rather than parked");
        assert_eq!(s.len(), 2, "and not counted either");
    }

    #[test]
    fn a_placeholder_somebody_else_is_still_in_is_not_dropped() {
        // The one thing the old parking rule could not express. Leaving a placeholder drops it,
        // because nobody wanted it — but "nobody" is a question about every terminal, so the view
        // that walked away says which conversation *it* left rather than the store assuming it was
        // the only one there.
        let (mut s, _a, b, _c) = store();
        let scratch = s.current_id().clone();
        s.get_mut(&scratch).expect("there").ephemeral = true;

        // A second terminal arrives in it from nowhere, then the first one leaves for `b`.
        s.enter(&scratch, None).expect("enter");
        go(&mut s, &scratch, &b);
        assert!(s.get(&scratch).is_none(), "one view leaving is still what drops it");
    }

    #[test]
    fn a_workspace_of_a_placeholder_and_an_archive_has_nothing_open() {
        // What `^F` is for on the empty transcript: everything archived reads as empty, because
        // what you have put away is not in the list you work from.
        let mut s = SessionStore::new(Session::new("/tmp"));
        let here = s.current_id().clone();
        s.get_mut(&here).expect("there").ephemeral = true;
        let away = Session::new("/tmp");
        let id = away.id.clone();
        s.insert(away);
        s.archive(&id, true, 1).expect("archives");
        assert!(!s.any_open());
        assert!(s.list_with(true).iter().any(|i| i.id == id), "still there to go back to");
    }

    #[test]
    fn stepping_away_never_lands_in_something_already_archived() {
        // The one destination that is definitely wrong is the conversation you put away.
        let (mut s, a, b, c) = store();
        s.archive(&b, true, 1).expect("archives b");
        assert_eq!(s.next_after(&a), Some(c.clone()), "not the archived one");
        s.archive(&c, true, 2).expect("archives c");
        assert_eq!(s.next_after(&a), None, "and nowhere left to go");
    }

    #[test]
    fn archiving_the_last_open_conversation_is_refused_rather_than_leaving_nowhere_to_type() {
        // The host answers this by starting an empty conversation first; the store's job is to
        // refuse rather than to invent one, because it cannot read a clock or a working directory.
        let mut s = SessionStore::new(Session::new("/x"));
        let only = s.current_id().clone();
        assert_eq!(s.archive(&only, true, 1), Err(StoreError::LastSession));
        assert!(!s.current().archived);
    }

    #[test]
    fn unarchiving_puts_it_back_at_the_top_because_that_is_why_you_did_it() {
        let (mut s, _a, _b, c) = store();
        s.archive(&c, true, 1).expect("archives");
        s.archive(&c, false, 0).expect("unarchives");
        assert_eq!(s.list().first().map(|i| i.id.clone()), Some(c.clone()));
        assert!(s.get(&c).expect("there").archived_at.is_none());
    }

    #[test]
    fn archiving_something_that_is_not_there_is_an_error_not_a_silent_success() {
        let (mut s, ..) = store();
        assert!(matches!(
            s.archive(&SessionId::from("nope"), true, 1),
            Err(StoreError::NoSuchSession(_))
        ));
    }

    #[test]
    fn a_new_store_has_exactly_one_session_and_it_is_the_current_one() {
        let s = SessionStore::new(Session::new("/x"));
        assert_eq!(s.len(), 1);
        assert_eq!(s.list().len(), 1);
        assert!(s.list()[0].is_active);
    }

    #[test]
    fn arriving_somewhere_leaves_the_conversation_behind_it_intact() {
        let (mut s, a, b, _c) = store();
        s.get_mut(&a).expect("there").push_user_text("first");
        go(&mut s, &a, &b);
        assert_eq!(s.current_id(), &b);
        assert!(s.current().messages.is_empty(), "the new session starts where it was");

        go(&mut s, &b, &a);
        assert_eq!(s.current().messages.len(), 1, "the one left behind survived");
    }

    #[test]
    fn arriving_at_a_conversation_is_what_reads_it() {
        // The mark is news, and news stops being news when you have been to look. Cleared here
        // rather than by whoever draws a list, so the panel, the status line and a picker cannot
        // disagree about whether you have seen something.
        let (mut s, a, b, _c) = store();
        s.get_mut(&b).expect("there").unread = true;
        assert!(s.list().iter().find(|i| i.id == b).expect("listed").unread);

        go(&mut s, &a, &b);
        assert!(!s.get(&b).expect("there").unread);
        assert!(s.list().iter().all(|i| !i.unread));

        // And it is only the one you arrived at. A "mark everything read" gesture nobody asked for
        // is how you lose the answer you switched away to wait for.
        s.get_mut(&a).expect("there").unread = true;
        s.enter(&b, Some(&b)).expect("already here");
        assert!(s.get(&a).expect("there").unread);
        assert!(s.mark_read(&a), "and it reports the change, so the caller knows to write it down");
        assert!(!s.mark_read(&a), "twice is not a change");
    }

    #[test]
    fn arriving_where_you_already_are_is_a_no_op_not_an_error() {
        let (mut s, a, _b, _c) = store();
        assert_eq!(s.enter(&a, Some(&a)), Ok(()));
        assert_eq!(s.current_id(), &a);
    }

    #[test]
    fn arriving_somewhere_that_does_not_exist_is_an_error() {
        let (mut s, _a, _b, _c) = store();
        let ghost = Session::new("/ghost").id;
        assert_eq!(s.enter(&ghost, None), Err(StoreError::NoSuchSession(ghost)));
    }

    #[test]
    fn the_list_is_most_recently_arrived_in_first() {
        let (mut s, a, b, c) = store();
        go(&mut s, &a, &c);
        go(&mut s, &c, &b);
        let ids: Vec<_> = s.list().into_iter().map(|i| i.id).collect();
        assert_eq!(ids[0], b, "the one arrived in last comes first");
        assert_eq!(ids[1], c, "then the one before it");
        assert!(ids.contains(&a));
        assert_eq!(s.list().iter().filter(|i| i.is_active).count(), 1);
    }

    #[test]
    fn every_session_appears_in_the_list_even_if_never_arrived_in() {
        // A session you cannot see is a session you cannot get back to.
        let (s, a, b, c) = store();
        let ids: Vec<_> = s.list().into_iter().map(|i| i.id).collect();
        for want in [a, b, c] {
            assert!(ids.contains(&want), "{want:?} missing from {ids:?}");
        }
    }

    #[test]
    fn closing_a_conversation_says_where_the_terminals_in_it_should_go() {
        let (mut s, a, b, c) = store();
        go(&mut s, &a, &c);
        go(&mut s, &c, &b);
        assert_eq!(s.next_after(&b), Some(c.clone()), "the one before it, not an arbitrary one");
        s.remove(&b).expect("close");
        assert_eq!(s.len(), 2);
        assert!(s.get(&b).is_none());
        assert!(s.get(&a).is_some());
    }

    #[test]
    fn closing_a_conversation_nobody_is_in_disturbs_nothing() {
        let (mut s, a, b, _c) = store();
        s.remove(&b).expect("close");
        assert_eq!(s.current_id(), &a);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn the_last_session_cannot_be_closed() {
        // There is always a conversation. A neosh with none has nowhere to put what you type next.
        let mut s = SessionStore::new(Session::new("/only"));
        let only = s.current_id().clone();
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
    fn inserting_a_session_that_is_already_there_replaces_it() {
        let mut s = SessionStore::new(Session::new("/x"));
        let id = s.current_id().clone();
        let mut replacement = Session::new("/x");
        replacement.id = id.clone();
        replacement.push_user_text("restored");
        s.insert(replacement);
        assert_eq!(s.len(), 1, "not duplicated");
        assert_eq!(s.current().messages.len(), 1);
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
