//! However many terminals you open, you see everything.
//!
//! ADR 0036 made the workspace a process and the terminal a viewer of it, and then allowed exactly
//! one viewer: a second `neosh` took the workspace over and the first was told why. That was the
//! honest thing to ship at the time and it is the wrong thing to keep — the reason to open a second
//! window is that a turn is running in the first, and taking it over is precisely the moment you
//! lose sight of it.
//!
//! # Views are mirrors of one workspace, not windows onto different parts of it
//!
//! There is still one `Editor`, so there is still one cursor, one scroll position and one composer.
//! Every attached terminal shows the same thing, and typing in either is typing in both. That is a
//! smaller claim than "independent windows" and it is the one the user asked for: what is on screen
//! here is what is on screen there, live, and closing one of them leaves the other alone.
//!
//! Independent views — a transcript here and a diff there — need `Host` split into a shared
//! `Workspace` and a per-view `View`, and need an `ApiCall` to name the view it draws into. This is
//! deliberately not that. It is the part that can be correct without a protocol change.
//!
//! # Size is the size of the smallest one
//!
//! A view resolves its own layout (ADR 0006), so two terminals of different sizes each lay out the
//! declarative geometry they are sent — that part was never the problem. What is shared is the
//! handful of places the *host* draws to a width because the thing being drawn is one row and must
//! stay one row: a tool card, the welcome. Those are buffer contents, identical in every view.
//!
//! So the workspace is told the smallest attached size and every view gets content that fits it.
//! The alternative is content measured for the widest terminal, which wraps in the narrow one — and
//! a card that wraps is the exact failure `chat_width` exists to avoid. A large window showing a
//! narrower transcript is a cost you can see and reason about; a corrupted one is not.
//!
//! Sizes are reconciled on every change *including a view leaving*, because a view that never
//! reports again is a view whose old size would otherwise pin the workspace narrow forever.

use std::collections::HashMap;

use neosh_proto::{InputEvent, ServerMessage, UiEvent, WindowId};
use tokio::sync::mpsc;

/// Which terminal something came from, or is going to.
///
/// Not on the wire. A view is a fact about *this* process's connections, and the client already
/// knows which one it is by virtue of being it — so nothing is gained by telling it a number, and
/// a number on the wire is one more thing to keep in step across an upgrade.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct ViewId(u64);

impl ViewId {
    /// The frontend of a process that is its own terminal — `--no-daemon`, `--ui-protocol=stdio`,
    /// every integration test that drives the host directly.
    ///
    /// There is exactly one and it never goes away, so nothing in here ever has to represent it.
    /// It exists so that "which view was that key from" has an answer in the one-process case too,
    /// rather than the host carrying an `Option` that is only ever `None` in half the builds.
    pub const LOCAL: ViewId = ViewId(0);
}

/// One attached terminal.
struct View {
    out: mpsc::UnboundedSender<ServerMessage>,
    /// What it said it was when it attached, and after every resize.
    size: (u16, u16),
    /// Whether this terminal is the window its user is looking at.
    ///
    /// `None` is *cannot say* rather than *no*: only a terminal implementing mode 1004 ever reports
    /// this, and treating silence as focused would mean everyone on a terminal that cannot report
    /// gets no notifications at all. Unknown falls back to [`View::last_input`]. See ADR 0057.
    focused: Option<bool>,
    /// When a key last arrived from this view, which is the fallback for a terminal that cannot say
    /// whether it has focus. Stamped on attach so a terminal that has just opened is not instantly
    /// idle.
    last_input: std::time::Instant,
    /// The geometry it resolved, per window, as last reported.
    ///
    /// Kept per view rather than merged on arrival because the merge has to be recomputed when a
    /// view *leaves*, and by then it is too late to ask the survivors what they think.
    viewports: HashMap<WindowId, (u16, u16, u32, u32)>,
}

/// Everyone looking at this workspace.
#[derive(Default)]
pub struct Views {
    next: u64,
    views: Vec<(ViewId, View)>,
    /// The merged viewport last forwarded to the host, per window, so an unchanged merge is not
    /// re-sent. Re-sending is not merely wasteful: `ViewportChanged` arms a redraw, which resolves
    /// geometry, which reports it again — the feedback loop `TerminalUi::reported` exists to break,
    /// and merging is a second place it can be re-tied.
    merged: HashMap<WindowId, (u16, u16, u32, u32)>,
}

impl Views {
    /// Take on a terminal. Its id is unique for the life of the workspace, so a view that leaves
    /// and comes back is a different one and cannot be confused with a message still in flight for
    /// the old one.
    pub fn attach(&mut self, out: mpsc::UnboundedSender<ServerMessage>, size: (u16, u16)) -> ViewId {
        self.next += 1;
        let id = ViewId(self.next);
        self.views.push((id, View {
            out,
            size,
            focused: None,
            last_input: std::time::Instant::now(),
            viewports: HashMap::new(),
        }));
        id
    }

    /// A terminal said whether it has focus.
    pub fn focused(&mut self, id: ViewId, on: bool) {
        if let Some((_, v)) = self.views.iter_mut().find(|(v, _)| *v == id) {
            v.focused = Some(on);
        }
    }

    /// A key arrived from this terminal, so somebody is at it.
    ///
    /// The fallback for terminals that cannot report focus, and it is also a correction for ones
    /// that can: a terminal you are typing into is a terminal you are looking at, whatever it last
    /// said about mode 1004. Terminals do get this wrong — a window manager that never sends the
    /// focus-in on the way back from a workspace switch leaves the view stuck reporting `false`,
    /// and then every notification is one for something you are staring at.
    pub fn typed(&mut self, id: ViewId) {
        if let Some((_, v)) = self.views.iter_mut().find(|(v, _)| *v == id) {
            v.last_input = std::time::Instant::now();
            if v.focused == Some(false) {
                v.focused = Some(true);
            }
        }
    }

    /// Whether nobody is looking at this workspace.
    ///
    /// A workspace is away only when *every* attached terminal is: two windows open and one of them
    /// in front means the thing is on a screen somebody can see. Nothing attached is not away — it
    /// is [`Views::is_empty`], which is a different answer with a different channel, because there
    /// is no terminal to write an escape to. See ADR 0057.
    pub fn away(&self, idle_after: std::time::Duration) -> bool {
        !self.views.is_empty()
            && self.views.iter().all(|(_, v)| match v.focused {
                Some(on) => !on,
                // A terminal that cannot say. Idleness is the honest proxy: the person who last
                // pressed a key ten minutes ago is not watching this scroll past.
                None => v.last_input.elapsed() >= idle_after,
            })
    }

    /// Let one go. `false` if it had already gone, which is ordinary: a client whose socket died
    /// and a client that said `Detach` are the same event arriving twice.
    pub fn detach(&mut self, id: ViewId) -> bool {
        let before = self.views.len();
        self.views.retain(|(v, _)| *v != id);
        self.views.len() != before
    }

    /// Tell one view something meant only for it — that it has attached, or that it is leaving.
    pub fn tell(&self, id: ViewId, msg: ServerMessage) {
        if let Some((_, v)) = self.views.iter().find(|(v, _)| *v == id) {
            let _ = v.out.send(msg);
        }
    }

    /// Draw the same frame on every terminal.
    ///
    /// A send failure is a client that went away between one lock and the next; the reader task
    /// notices and detaches it. Not a reason to stop drawing on the others, and certainly not a
    /// reason to end a turn.
    pub fn send(&self, batch: Vec<UiEvent>) {
        for (_, v) in &self.views {
            let _ = v.out.send(ServerMessage::Events { batch: batch.clone() });
        }
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    /// The size the workspace should draw for: the smallest of every attached terminal, in each
    /// dimension independently. A tall narrow window beside a short wide one leaves a workspace
    /// that is narrow *and* short, which is the only size whose content fits in both.
    pub fn size(&self) -> Option<(u16, u16)> {
        self.views
            .iter()
            .map(|(_, v)| v.size)
            .reduce(|(aw, ah), (bw, bh)| (aw.min(bw), ah.min(bh)))
    }

    /// Record a view's new size. `Some` if the *workspace's* size changed as a result — a view
    /// growing while a smaller one is still attached changes nothing anybody has to redraw.
    pub fn resized(&mut self, id: ViewId, size: (u16, u16)) -> Option<(u16, u16)> {
        let before = self.size();
        if let Some((_, v)) = self.views.iter_mut().find(|(v, _)| *v == id) {
            v.size = size;
        }
        let now = self.size();
        (now != before).then_some(now).flatten()
    }

    /// Record what one view resolved for a window, and return the merged geometry if the merge
    /// moved. Width and height are the smallest reported; `top_line` comes from the same view as
    /// the smallest height, because a scroll position paired with somebody else's viewport is a
    /// number about a screen that does not exist.
    pub fn viewport(
        &mut self,
        id: ViewId,
        win: WindowId,
        wht: (u16, u16, u32, u32),
    ) -> Option<InputEvent> {
        if let Some((_, v)) = self.views.iter_mut().find(|(v, _)| *v == id) {
            v.viewports.insert(win, wht);
        }
        self.merge(win)
    }

    /// Recompute every window's merged geometry, for when the set of views changed rather than one
    /// view's opinion of it.
    pub fn remerge(&mut self) -> Vec<InputEvent> {
        let wins: Vec<WindowId> =
            self.views.iter().flat_map(|(_, v)| v.viewports.keys().copied()).collect();
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
        for (_, v) in &self.views {
            let Some(&(w, h, top, rows)) = v.viewports.get(&win) else { continue };
            best = Some(match best {
                None => (w, h, top, rows),
                // The narrowest width, the shortest height, and the `top_line` and row count
                // belonging to whichever view is shortest — that is the one a row has to be
                // visible in, and the two of them describe one screen and have to come from it.
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

    fn view(views: &mut Views, size: (u16, u16)) -> (ViewId, mpsc::UnboundedReceiver<ServerMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (views.attach(tx, size), rx)
    }

    fn drew(rx: &mut mpsc::UnboundedReceiver<ServerMessage>) -> usize {
        let mut n = 0;
        while let Ok(ServerMessage::Events { .. }) = rx.try_recv() {
            n += 1;
        }
        n
    }

    #[test]
    fn a_frame_is_drawn_on_every_terminal() {
        let mut views = Views::default();
        let (_, mut a) = view(&mut views, (100, 40));
        let (_, mut b) = view(&mut views, (80, 24));

        views.send(vec![UiEvent::Flush]);

        assert_eq!(drew(&mut a), 1);
        assert_eq!(drew(&mut b), 1, "the second terminal saw nothing");
    }

    #[test]
    fn the_workspace_is_the_size_of_the_smallest_terminal() {
        let mut views = Views::default();
        let (_, _a) = view(&mut views, (100, 40));
        assert_eq!(views.size(), Some((100, 40)));

        let (b, _b) = view(&mut views, (80, 50));
        assert_eq!(views.size(), Some((80, 40)), "narrow from one, short from the other");

        // The one that was pinning the width goes away, and the workspace gets it back. Nobody
        // reports a size when somebody *else* closes a window, so this has to be recomputed here
        // or the workspace stays narrow for as long as it runs.
        assert!(views.detach(b));
        assert_eq!(views.size(), Some((100, 40)));
    }

    #[test]
    fn a_view_growing_beside_a_smaller_one_changes_nothing() {
        let mut views = Views::default();
        let (a, _a) = view(&mut views, (100, 40));
        let (_, _b) = view(&mut views, (80, 24));

        assert_eq!(views.resized(a, (200, 90)), None, "still bounded by the small one");
        assert_eq!(views.resized(a, (60, 40)), Some((60, 24)), "now it is the small one");
    }

    #[test]
    fn the_last_terminal_closing_leaves_a_workspace_with_no_size_and_no_opinion() {
        let mut views = Views::default();
        let (a, _a) = view(&mut views, (100, 40));
        assert!(views.detach(a));
        assert!(views.is_empty());
        // Not `Some((0, 0))`: nobody is looking, so there is no size, and the host keeps whatever
        // it last drew for. A zero would be a width every card had to defend against.
        assert_eq!(views.size(), None);
    }

    #[test]
    fn a_window_is_as_big_as_the_smallest_view_of_it() {
        let mut views = Views::default();
        let (a, _a) = view(&mut views, (100, 40));
        let (b, _b) = view(&mut views, (80, 24));
        let win = WindowId(1);

        assert_eq!(
            views.viewport(a, win, (100, 38, 12, 38)),
            Some(InputEvent::ViewportChanged { win, width: 100, height: 38, top_line: 12, rows: 38 })
        );
        assert_eq!(
            views.viewport(b, win, (80, 22, 5, 22)),
            Some(InputEvent::ViewportChanged { win, width: 80, height: 22, top_line: 5, rows: 22 }),
            "the shortest view's own scroll position, not a mix of two"
        );
        assert_eq!(views.viewport(b, win, (80, 22, 5, 22)), None, "an unchanged merge is not re-sent");
    }

    #[test]
    fn a_view_leaving_gives_the_window_back_to_the_survivors() {
        let mut views = Views::default();
        let (a, _a) = view(&mut views, (100, 40));
        let (b, _b) = view(&mut views, (80, 24));
        let win = WindowId(1);
        views.viewport(a, win, (100, 38, 12, 38));
        views.viewport(b, win, (80, 22, 5, 22));

        assert!(views.detach(b));

        assert_eq!(
            views.remerge(),
            vec![InputEvent::ViewportChanged { win, width: 100, height: 38, top_line: 12, rows: 38 }],
            "the survivor's geometry, said again because nothing else will say it"
        );
        assert!(views.remerge().is_empty(), "and only once");
    }

    #[test]
    fn a_view_that_already_went_is_not_detached_twice() {
        let mut views = Views::default();
        let (a, _a) = view(&mut views, (100, 40));
        assert!(views.detach(a));
        assert!(!views.detach(a), "a dead socket and an asked-for detach are the same event twice");
    }
}
