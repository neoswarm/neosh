//! Syntax colour a transcript could not wait for.
//!
//! Highlighting is the slow part of drawing a conversation: `fancy-regex` through a TypeScript
//! grammar is about a hundred and fifty microseconds a line, a long conversation carries a few
//! thousand lines of diff, and the first line of each language also compiles its patterns. Done
//! on the host loop, that was most of a second — and on a busy machine three — between pressing
//! `↵` on a conversation and seeing it, with every key in every terminal waiting behind it.
//!
//! So a rebuild asks under [`neosh_syntax::deferring`]: whatever the memo already holds is drawn
//! in colour, the rest is drawn plain, and what was missing comes here. One thread does it, the
//! newest first because that is the bottom of the transcript and the part on screen, and says
//! which conversation it finished so the host can colour the cards it drew plain. The text of a
//! row does not depend on its colour, so nothing moves when it lands.
//!
//! The thread also fills the memo for conversations nobody has opened yet ([`Colourist::build`]),
//! which is what makes the *first* switch into one fast too. That work always yields to the
//! conversation somebody is looking at: a [`Colourist::colour`] is taken before the next piece of
//! it.

use std::collections::VecDeque;
use std::sync::mpsc;

use neosh_proto::SessionId;
use neosh_syntax::Pending;

/// How many of a rebuild's highlights are done, and announced, before the rest: about a screen's
/// worth of cards, so what is in view colours as soon as it can rather than when the whole
/// conversation has.
const FIRST: usize = 8;

/// How many of a background conversation's highlights are done between looks at the queue, so a
/// conversation somebody has just opened never waits behind one nobody has.
const CHUNK: usize = 16;

/// Work for the colouring thread.
enum Job {
    /// Highlight these, and say so for this conversation.
    Run(SessionId, Pending),
    /// Find out what this conversation would need highlighting, and highlight it. Nothing is said
    /// afterwards: nothing on screen was drawn from it.
    Build(Build),
}

type Build = Box<dyn FnOnce() -> Pending + Send>;

/// The handle the host keeps.
pub struct Colourist {
    tx: mpsc::Sender<Job>,
}

impl Colourist {
    /// Start the thread. `done` hears a conversation each time some of its highlighting landed.
    pub fn spawn(done: tokio::sync::mpsc::UnboundedSender<SessionId>) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let _ = std::thread::Builder::new().name("neosh-colour".into()).spawn(move || work(&rx, &done));
        Self { tx }
    }

    /// Highlight what a rebuild drew plain, the part on screen first.
    pub fn colour(&self, session: SessionId, mut pending: Pending) {
        if pending.is_empty() {
            return;
        }
        let first = pending.split_newest(FIRST);
        let _ = self.tx.send(Job::Run(session.clone(), first));
        if !pending.is_empty() {
            let _ = self.tx.send(Job::Run(session, pending));
        }
    }

    /// Fill the memo for a conversation that is not on screen, whenever nothing that is needs it.
    pub fn build(&self, f: impl FnOnce() -> Pending + Send + 'static) {
        let _ = self.tx.send(Job::Build(Box::new(f)));
    }
}

fn work(rx: &mpsc::Receiver<Job>, done: &tokio::sync::mpsc::UnboundedSender<SessionId>) {
    let mut runs: VecDeque<(SessionId, Pending)> = VecDeque::new();
    let mut builds: VecDeque<Build> = VecDeque::new();
    let mut background: VecDeque<Pending> = VecDeque::new();
    // Sorted by whether somebody is looking at it.
    fn take(job: Job, runs: &mut VecDeque<(SessionId, Pending)>, builds: &mut VecDeque<Build>) {
        match job {
            Job::Run(session, pending) => runs.push_back((session, pending)),
            Job::Build(f) => builds.push_back(f),
        }
    }
    loop {
        while let Ok(job) = rx.try_recv() {
            take(job, &mut runs, &mut builds);
        }
        if let Some((session, pending)) = runs.pop_front() {
            pending.run();
            if done.send(session).is_err() {
                return;
            }
            continue;
        }
        if let Some(mut pending) = background.pop_front() {
            let piece = pending.split_newest(CHUNK);
            if !pending.is_empty() {
                background.push_front(pending);
            }
            piece.run();
            continue;
        }
        if let Some(f) = builds.pop_front() {
            let pending = f();
            if !pending.is_empty() {
                background.push_back(pending);
            }
            continue;
        }
        // Nothing to do: wait for something, and stop when the host has gone.
        match rx.recv() {
            Ok(job) => take(job, &mut runs, &mut builds),
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_run_is_announced_for_its_conversation_and_fills_the_memo() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let colourist = Colourist::spawn(tx);
        let lang = neosh_syntax::of_token("rust").expect("rust");
        let code = "fn coloured_off_the_loop() -> u8 { 7 }\n";
        let (plain, pending) = neosh_syntax::deferring(|| neosh_syntax::spans(lang, code));
        assert!(plain.is_none());
        let id = SessionId("s".into());
        colourist.colour(id.clone(), pending);
        let said = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await;
        assert_eq!(said.ok().flatten(), Some(id));
        let (now, none) = neosh_syntax::deferring(|| neosh_syntax::spans(lang, code));
        assert!(now.is_some() && none.is_empty());
    }

    #[tokio::test]
    async fn a_build_says_nothing_and_still_fills_the_memo() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let colourist = Colourist::spawn(tx);
        let lang = neosh_syntax::of_token("rust").expect("rust");
        let code = "fn built_in_the_background() {}\n";
        colourist.build(move || neosh_syntax::deferring(|| neosh_syntax::spans(lang, code)).1);
        // A run queued after it is announced, so by then the build has been taken too — or is
        // being worked through; either way, wait until the memo has it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (now, _) = neosh_syntax::deferring(|| neosh_syntax::spans(lang, code));
            if now.is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "the build never landed");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(rx.try_recv().is_err(), "a build is not news");
    }
}
