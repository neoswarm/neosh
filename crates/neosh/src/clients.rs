//! However many terminals you open, you see everything.
//!
//! Making the workspace a process and the terminal a viewer of it at first allowed exactly
//! one viewer: a second `neosh` took the workspace over and the first was told why. That was the
//! honest thing to ship at the time and it is the wrong thing to keep — the reason to open a second
//! window is that a turn is running in the first, and taking it over is precisely the moment you
//! lose sight of it.
//!
//! # A client is a socket; a view is a place in the workspace
//!
//! Two different things, and they used to be one because there was only ever one place to be. A
//! [`ClientId`] is a terminal on the end of a connection: it has a size, it reports the geometry it
//! resolved, and `^Q` closes it. A [`ViewId`] is a set of windows — the conversation on screen, the
//! scroll position, the composer, the panels open over it.
//!
//! Every client looks at exactly one view. Several clients may look at the *same* view, which is
//! what shipped first: mirrors, identical and live. What is being built on top of this is a view
//! per client, so that a second terminal is somewhere else rather than a copy — and keeping the two
//! ids apart is what lets that arrive one piece at a time instead of all at once.
//!
//! Buffers are not part of a view. What the agent produced is the workspace's, so a conversation
//! open in two terminals is one transcript with two cursors on it — which is exactly what a window
//! has always been (`neosh_core::window::Window` holds the cursor, the anchor and the scroll).
//!
//! # Every terminal is its own size again
//!
//! A client resolves its own layout, so two terminals of different sizes each lay out
//! the declarative geometry they are sent — that part was never the problem. What was, was the
//! handful of places the *host* draws to a width because the thing being drawn is one row and must
//! stay one row: a tool card, the welcome. Those were buffer contents, one copy shared by every
//! view, so the workspace had to be told the smallest attached size and a wide terminal drew a
//! narrow transcript.
//!
//! A transcript is per view now, so there is nothing left to share and nothing to clamp: a card in
//! the wide window is fitted to the wide window and the same card next door is elided to fit the
//! narrow one. What is still merged is the geometry of one *window*, because two clients may be
//! looking at the same view — and there the old reasoning holds exactly, since it is one window and
//! it has to fit in both. That is reconciled on every change *including a client leaving*, because
//! a client that never reports again is one whose old size would otherwise pin the window narrow
//! for as long as the workspace runs.

use std::collections::HashMap;

use neosh_proto::{InputEvent, ServerMessage, UiEvent, ViewId, WindowId};
use tokio::sync::mpsc;

/// Which terminal something came from, or is going to.
///
/// Not on the wire, and deliberately: the client already knows which one it is by virtue of being
/// it, so telling it a number buys nothing and costs one more field to keep in step across an
/// upgrade. [`ViewId`] *is* on the wire, because a plugin has to be able to say which set of
/// windows it is drawing into.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ClientId(u32);

impl ClientId {
    /// The frontend of a process that is its own terminal — `--no-daemon`, `--ui-protocol=stdio`,
    /// every integration test that drives the host directly.
    ///
    /// There is exactly one and it never goes away, so nothing in here ever has to represent it.
    /// It exists so that "which terminal was that key from" has an answer in the one-process case
    /// too, rather than the host carrying an `Option` that is only ever `None` in half the builds.
    pub const LOCAL: ClientId = ClientId(0);
}

/// Where an input event came from.
///
/// Both halves, because both are asked about and they are different questions. `^Q` closes the
/// *client* — one terminal, and never somebody else's. A key that scrolls, switches conversation or
/// opens a panel acts on the *view*, which two terminals may be sharing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Source {
    pub client: ClientId,
    pub view: ViewId,
}

impl Source {
    /// A process that is its own terminal, and anything the host does on nobody's behalf — a
    /// signal, a timer, a command a plugin ran.
    pub const LOCAL: Source = Source { client: ClientId::LOCAL, view: ViewId::LOCAL };
}

/// One frame, as the host produced it: every event, each tagged with the view it concerns.
///
/// `None` is everybody. Buffer contents, highlights and messages are the workspace's and go to
/// every terminal; anything shaped like a window — where the cursor is, what is scrolled where,
/// which float is on top — belongs to one view and is sent only there.
pub type Frame = Vec<(Option<ViewId>, UiEvent)>;

/// One attached terminal.
struct Client {
    out: mpsc::UnboundedSender<ServerMessage>,
    /// Which set of windows it is looking at.
    view: ViewId,
    /// What it said it was when it attached, and after every resize.
    size: (u16, u16),
    /// Whether this terminal is the window its user is looking at.
    ///
    /// `None` is *cannot say* rather than *no*: only a terminal implementing mode 1004 ever reports
    /// this, and treating silence as focused would mean everyone on a terminal that cannot report
    /// gets no notifications at all. Unknown falls back to [`Client::last_input`].
    focused: Option<bool>,
    /// When a key last arrived from this terminal, which is the fallback for one that cannot say
    /// whether it has focus. Stamped on attach so a terminal that has just opened is not instantly
    /// idle.
    last_input: std::time::Instant,
    /// The geometry it resolved, per window, as last reported.
    ///
    /// Kept per client rather than merged on arrival because the merge has to be recomputed when a
    /// client *leaves*, and by then it is too late to ask the survivors what they think.
    viewports: HashMap<WindowId, (u16, u16, u32, u32)>,
}

/// Everyone looking at this workspace.
#[derive(Default)]
pub struct Clients {
    next: u32,
    /// Ids handed to views. Separate from the client counter so that the two never look
    /// interchangeable in a log, and so that a future "mirror that terminal" can hand out a view
    /// somebody already has.
    next_view: u32,
    clients: Vec<(ClientId, Client)>,
    /// The merged viewport last forwarded to the host, per window, so an unchanged merge is not
    /// re-sent. Re-sending is not merely wasteful: `ViewportChanged` arms a redraw, which resolves
    /// geometry, which reports it again — the feedback loop `TerminalUi::reported` exists to break,
    /// and merging is a second place it can be re-tied.
    merged: HashMap<WindowId, (u16, u16, u32, u32)>,
}

impl Clients {
    /// Take on a terminal, with a view of its own. Both ids are unique for the life of the
    /// workspace, so a client that leaves and comes back is a different one and cannot be confused
    /// with a message still in flight for the old one.
    pub fn attach(
        &mut self,
        out: mpsc::UnboundedSender<ServerMessage>,
        size: (u16, u16),
    ) -> Source {
        self.next += 1;
        self.next_view += 1;
        let id = ClientId(self.next);
        let view = ViewId(self.next_view);
        self.clients.push((id, Client {
            out,
            view,
            size,
            focused: None,
            last_input: std::time::Instant::now(),
            viewports: HashMap::new(),
        }));
        Source { client: id, view }
    }

    /// Take on a terminal looking at a view that already exists — a mirror of another one.
    ///
    /// Nothing calls this yet. It is here because the whole reason a client and a view are two
    /// ids is that the relationship is not forced to be one to one, and a test that shares one
    /// proves the routing does not quietly assume otherwise.
    #[cfg(test)]
    pub fn attach_to(
        &mut self,
        out: mpsc::UnboundedSender<ServerMessage>,
        size: (u16, u16),
        view: ViewId,
    ) -> ClientId {
        self.next += 1;
        let id = ClientId(self.next);
        self.clients.push((id, Client {
            out,
            view,
            size,
            focused: None,
            last_input: std::time::Instant::now(),
            viewports: HashMap::new(),
        }));
        id
    }

    /// Let one go. `false` if it had already gone, which is ordinary: a client whose socket died
    /// and a client that said `Detach` are the same event arriving twice.
    pub fn detach(&mut self, id: ClientId) -> bool {
        let before = self.clients.len();
        self.clients.retain(|(c, _)| *c != id);
        self.clients.len() != before
    }

    /// Which set of windows a terminal is looking at.
    ///
    /// Test-only for the same reason [`attach_to`](Self::attach_to) is: a connection is handed its
    /// [`Source`] when it attaches and keeps it for its life, so nothing in the workspace has to
    /// look the pairing back up. What the tests are for is that the pairing is what it says.
    #[cfg(test)]
    pub fn view_of(&self, id: ClientId) -> Option<ViewId> {
        self.clients.iter().find(|(c, _)| *c == id).map(|(_, c)| c.view)
    }

    /// A terminal said whether it has focus.
    pub fn focused(&mut self, id: ClientId, on: bool) {
        if let Some((_, c)) = self.clients.iter_mut().find(|(c, _)| *c == id) {
            c.focused = Some(on);
        }
    }

    /// A key arrived from this terminal, so somebody is at it.
    ///
    /// The fallback for terminals that cannot report focus, and it is also a correction for ones
    /// that can: a terminal you are typing into is a terminal you are looking at, whatever it last
    /// said about mode 1004. Terminals do get this wrong — a window manager that never sends the
    /// focus-in on the way back from a workspace switch leaves the client stuck reporting `false`,
    /// and then every notification is one for something you are staring at.
    pub fn typed(&mut self, id: ClientId) {
        if let Some((_, c)) = self.clients.iter_mut().find(|(c, _)| *c == id) {
            c.last_input = std::time::Instant::now();
            if c.focused == Some(false) {
                c.focused = Some(true);
            }
        }
    }

    /// Whether nobody is looking at this workspace.
    ///
    /// A workspace is away only when *every* attached terminal is: two windows open and one of them
    /// in front means the thing is on a screen somebody can see. Nothing attached is not away — it
    /// is [`Clients::is_empty`], which is a different answer with a different channel, because
    /// there is no terminal to write an escape to.
    pub fn away(&self, idle_after: std::time::Duration) -> bool {
        !self.clients.is_empty()
            && self.clients.iter().all(|(_, c)| match c.focused {
                Some(on) => !on,
                // A terminal that cannot say. Idleness is the honest proxy: the person who last
                // pressed a key ten minutes ago is not watching this scroll past.
                None => c.last_input.elapsed() >= idle_after,
            })
    }

    /// Whether anybody is still looking at a view. A view nothing is attached to is one the host
    /// can take down.
    pub fn watching(&self, view: ViewId) -> bool {
        self.clients.iter().any(|(_, c)| c.view == view)
    }

    /// Tell one client something meant only for it — that it has attached, or that it is leaving.
    pub fn tell(&self, id: ClientId, msg: ServerMessage) {
        if let Some((_, c)) = self.clients.iter().find(|(c, _)| *c == id) {
            let _ = c.out.send(msg);
        }
    }

    /// Draw one frame, giving every terminal the part of it that is about where *it* is looking.
    ///
    /// A client whose share is nothing but the trailing `Flush` is sent nothing at all: a flush is
    /// a redraw, and a terminal that would repaint an unchanged screen twenty times a second
    /// because something happened in somebody else's conversation is the cost this routing exists
    /// to avoid.
    ///
    /// A send failure is a client that went away between one lock and the next; the reader task
    /// notices and detaches it. Not a reason to stop drawing on the others, and certainly not a
    /// reason to end a turn.
    pub fn send(&self, frame: &Frame) {
        for (_, c) in &self.clients {
            let mut batch: Vec<UiEvent> = Vec::with_capacity(frame.len());
            let mut theirs = false;
            for (view, ev) in frame {
                match view {
                    Some(v) if *v != c.view => continue,
                    Some(_) => theirs = true,
                    None => theirs |= !matches!(ev, UiEvent::Flush),
                }
                batch.push(ev.clone());
            }
            if !theirs {
                continue;
            }
            let _ = c.out.send(ServerMessage::Events { batch });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.clients.is_empty()
    }

    /// Record a terminal's new size, and say what to tell the host.
    ///
    /// Its own, always. It used to be the smallest of every attached terminal, because a tool card
    /// was one row of content shared by all of them; a transcript belongs to a view now, so a
    /// terminal being resized is a fact about that terminal and nobody else's screen has to move.
    pub fn resized(&mut self, id: ClientId, size: (u16, u16)) -> Option<(u16, u16)> {
        let Some((_, c)) = self.clients.iter_mut().find(|(c, _)| *c == id) else { return None };
        c.size = size;
        Some(size)
    }

    /// Record what one client resolved for a window, and return the merged geometry if the merge
    /// moved.
    ///
    /// A window belongs to one view, so nearly always there is one client reporting and the merge
    /// is a copy. When two terminals are looking at the *same* view they both report it, and then
    /// it is one window that has to fit in both: width and height are the smallest reported, and
    /// `top_line` comes from the same client as the smallest height, because a scroll position
    /// paired with somebody else's viewport is a number about a screen that does not exist.
    pub fn viewport(
        &mut self,
        id: ClientId,
        win: WindowId,
        wht: (u16, u16, u32, u32),
    ) -> Option<InputEvent> {
        if let Some((_, c)) = self.clients.iter_mut().find(|(c, _)| *c == id) {
            c.viewports.insert(win, wht);
        }
        self.merge(win)
    }

    /// Recompute every window's merged geometry, for when the set of clients changed rather than
    /// one client's opinion of it.
    pub fn remerge(&mut self) -> Vec<InputEvent> {
        let wins: Vec<WindowId> =
            self.clients.iter().flat_map(|(_, c)| c.viewports.keys().copied()).collect();
        let mut seen = Vec::new();
        let mut out = Vec::new();
        for win in wins {
            if seen.contains(&win) {
                continue;
            }
            seen.push(win);
            if let Some(ev) = self.merge(win) {
                out.push(ev);
            }
        }
        out
    }

    fn merge(&mut self, win: WindowId) -> Option<InputEvent> {
        let mut best: Option<(u16, u16, u32, u32)> = None;
        for (_, c) in &self.clients {
            let Some(&(w, h, top, rows)) = c.viewports.get(&win) else { continue };
            best = Some(match best {
                None => (w, h, top, rows),
                // The narrowest width, the shortest height, and the `top_line` and row count
                // belonging to whichever client is shortest — that is the one a row has to be
                // visible in, and the two of them describe one screen and have to come from it.
                // Only ever reached by two terminals sharing a view.
                Some((bw, bh, btop, brows)) => (
                    bw.min(w),
                    bh.min(h),
                    if h < bh { top } else { btop },
                    if h < bh { rows } else { brows },
                ),
            });
        }
        let (width, height, top_line, rows) = best?;
        if self.merged.get(&win) == Some(&(width, height, top_line, rows)) {
            return None;
        }
        self.merged.insert(win, (width, height, top_line, rows));
        Some(InputEvent::ViewportChanged { win, width, height, top_line, rows })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(
        clients: &mut Clients,
        size: (u16, u16),
    ) -> (ClientId, mpsc::UnboundedReceiver<ServerMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (clients.attach(tx, size).client, rx)
    }

    fn seeing(
        clients: &mut Clients,
        size: (u16, u16),
        view: ViewId,
    ) -> (ClientId, mpsc::UnboundedReceiver<ServerMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (clients.attach_to(tx, size, view), rx)
    }

    /// Every event of every batch this terminal was sent.
    fn drew(rx: &mut mpsc::UnboundedReceiver<ServerMessage>) -> Vec<UiEvent> {
        let mut out = Vec::new();
        while let Ok(ServerMessage::Events { batch }) = rx.try_recv() {
            out.extend(batch);
        }
        out
    }

    fn frame(events: Vec<(Option<ViewId>, UiEvent)>) -> Frame {
        let mut f = events;
        f.push((None, UiEvent::Flush));
        f
    }

    fn opened(win: u32) -> UiEvent {
        UiEvent::WindowOpened {
            win: WindowId(win),
            buf: neosh_proto::BufferId(1),
            layout: neosh_proto::WindowLayout::Docked { pane: None,
                dock: neosh_proto::Dock::Main,
                size: None,
                gravity: Default::default(),
                wrap: None,
            },
        }
    }

    #[test]
    fn a_frame_is_drawn_on_every_terminal() {
        let mut clients = Clients::default();
        let (_, mut a) = client(&mut clients, (100, 40));
        let (_, mut b) = client(&mut clients, (80, 24));

        // Untagged: a buffer is the workspace's, so it reaches both however far apart they are.
        clients.send(&frame(vec![(None, opened(1))]));

        assert_eq!(drew(&mut a).len(), 2);
        assert_eq!(drew(&mut b).len(), 2, "the second terminal saw nothing");
    }

    #[test]
    fn a_window_in_one_view_is_not_drawn_in_another() {
        let mut clients = Clients::default();
        let (_, mut a) = seeing(&mut clients, (100, 40), ViewId(1));
        let (_, mut b) = seeing(&mut clients, (100, 40), ViewId(2));

        clients.send(&frame(vec![
            (None, UiEvent::BufferOpened { buf: neosh_proto::BufferId(1), name: "[chat]".into() }),
            (Some(ViewId(1)), opened(7)),
        ]));

        let a = drew(&mut a);
        let b = drew(&mut b);
        assert!(a.contains(&opened(7)), "the view it belongs to did not get its own window");
        assert!(!b.contains(&opened(7)), "somebody else's window arrived here");
        assert_eq!(b.len(), 2, "but the buffer is the workspace's, and the flush with it");
    }

    #[test]
    fn a_terminal_with_nothing_to_draw_is_not_woken_up() {
        let mut clients = Clients::default();
        let (_, mut a) = seeing(&mut clients, (100, 40), ViewId(1));
        let (_, mut b) = seeing(&mut clients, (100, 40), ViewId(2));

        clients.send(&frame(vec![(Some(ViewId(1)), opened(7))]));

        assert_eq!(drew(&mut a).len(), 2);
        assert!(
            drew(&mut b).is_empty(),
            "a lone flush is a full repaint of a screen that did not change"
        );
    }

    #[test]
    fn two_terminals_can_share_one_view() {
        let mut clients = Clients::default();
        let (_, mut a) = seeing(&mut clients, (100, 40), ViewId(1));
        let (_, mut b) = seeing(&mut clients, (80, 24), ViewId(1));

        clients.send(&frame(vec![(Some(ViewId(1)), opened(7))]));

        assert!(drew(&mut a).contains(&opened(7)));
        assert!(drew(&mut b).contains(&opened(7)), "a mirror is two clients on one view");
    }

    #[test]
    fn a_view_is_watched_until_the_last_terminal_on_it_goes() {
        let mut clients = Clients::default();
        let (a, _a) = seeing(&mut clients, (100, 40), ViewId(1));
        let (b, _b) = seeing(&mut clients, (80, 24), ViewId(1));
        let (_c, _crx) = seeing(&mut clients, (80, 24), ViewId(2));

        clients.detach(a);
        assert!(clients.watching(ViewId(1)), "the other terminal is still on it");
        clients.detach(b);
        assert!(!clients.watching(ViewId(1)), "and now nobody is");
        assert!(clients.watching(ViewId(2)));
    }

    #[test]
    fn every_terminal_gets_a_view_of_its_own() {
        let mut clients = Clients::default();
        let (tx, _rx) = mpsc::unbounded_channel();
        let a = clients.attach(tx, (100, 40));
        let (tx, _rx) = mpsc::unbounded_channel();
        let b = clients.attach(tx, (80, 24));

        assert_ne!(a.view, b.view, "two terminals are two places");
        assert_ne!(a.client, b.client);
        assert_eq!(clients.view_of(a.client), Some(a.view));

        clients.detach(a.client);
        assert_eq!(clients.view_of(a.client), None);
        assert_eq!(clients.view_of(b.client), Some(b.view));
    }

    #[test]
    fn a_terminal_is_its_own_size_whoever_else_is_attached() {
        // It used to be the smallest of all of them, because a card was one row of content every
        // terminal shared. A transcript belongs to a view now, so a wide window is wide.
        let mut clients = Clients::default();
        let (a, _a) = client(&mut clients, (100, 40));
        let (_, _b) = client(&mut clients, (60, 20));

        assert_eq!(clients.resized(a, (200, 90)), Some((200, 90)), "not clamped to the small one");
        assert_eq!(clients.resized(a, (60, 40)), Some((60, 40)));
    }

    #[test]
    fn a_terminal_that_has_gone_reports_nothing() {
        let mut clients = Clients::default();
        let (a, _a) = client(&mut clients, (100, 40));
        assert!(clients.detach(a));
        assert!(clients.is_empty());
        assert_eq!(clients.resized(a, (80, 24)), None);
    }

    #[test]
    fn a_window_two_terminals_share_is_as_big_as_the_smaller_of_them() {
        let mut clients = Clients::default();
        let (a, _a) = seeing(&mut clients, (100, 40), ViewId(1));
        let (b, _b) = seeing(&mut clients, (80, 24), ViewId(1));
        let win = WindowId(1);

        assert_eq!(
            clients.viewport(a, win, (100, 38, 12, 38)),
            Some(InputEvent::ViewportChanged { win, width: 100, height: 38, top_line: 12, rows: 38 })
        );
        assert_eq!(
            clients.viewport(b, win, (80, 22, 5, 22)),
            Some(InputEvent::ViewportChanged { win, width: 80, height: 22, top_line: 5, rows: 22 }),
            "the shortest view's own scroll position, not a mix of two"
        );
        assert_eq!(
            clients.viewport(b, win, (80, 22, 5, 22)),
            None,
            "an unchanged merge is not re-sent"
        );
    }

    #[test]
    fn a_terminal_leaving_gives_the_window_back_to_the_survivors() {
        let mut clients = Clients::default();
        let (a, _a) = seeing(&mut clients, (100, 40), ViewId(1));
        let (b, _b) = seeing(&mut clients, (80, 24), ViewId(1));
        let win = WindowId(1);
        clients.viewport(a, win, (100, 38, 12, 38));
        clients.viewport(b, win, (80, 22, 5, 22));

        assert!(clients.detach(b));

        assert_eq!(
            clients.remerge(),
            vec![InputEvent::ViewportChanged { win, width: 100, height: 38, top_line: 12, rows: 38 }],
            "the survivor's geometry, said again because nothing else will say it"
        );
        assert!(clients.remerge().is_empty(), "and only once");
    }

    #[test]
    fn a_terminal_that_already_went_is_not_detached_twice() {
        let mut clients = Clients::default();
        let (a, _a) = client(&mut clients, (100, 40));
        assert!(clients.detach(a));
        assert!(!clients.detach(a), "a dead socket and an asked-for detach are the same event twice");
    }
}
