//! The focus stack.
//!
//! Whoever is on top gets first refusal on a key, then the focused window's buffer, then global
//! maps. Without this, every plugin fights over `<Esc>` — which matters more here than in an editor
//! because `<Esc>` is also the interrupt key for a running agent turn.

use neosh_proto::WindowId;

#[derive(Debug, Default, Clone)]
pub struct FocusStack {
    stack: Vec<WindowId>,
}

impl FocusStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, win: WindowId) {
        // Re-focusing something already in the stack raises it rather than duplicating it,
        // otherwise a popped entry would reveal a stale copy of itself.
        self.stack.retain(|w| *w != win);
        self.stack.push(win);
    }

    pub fn pop(&mut self) -> Option<WindowId> {
        self.stack.pop()
    }

    pub fn current(&self) -> Option<WindowId> {
        self.stack.last().copied()
    }

    /// Called when a window is destroyed. A closed window must not stay focusable.
    pub fn remove(&mut self, win: WindowId) {
        self.stack.retain(|w| *w != win);
    }

    pub fn contains(&self, win: WindowId) -> bool {
        self.stack.contains(&win)
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn iter_top_down(&self) -> impl Iterator<Item = WindowId> + '_ {
        self.stack.iter().rev().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_is_last_in_first_out() {
        let mut f = FocusStack::new();
        f.push(WindowId(1));
        f.push(WindowId(2));
        assert_eq!(f.current(), Some(WindowId(2)));
        assert_eq!(f.pop(), Some(WindowId(2)));
        assert_eq!(f.current(), Some(WindowId(1)));
    }

    #[test]
    fn refocusing_raises_rather_than_duplicates() {
        let mut f = FocusStack::new();
        f.push(WindowId(1));
        f.push(WindowId(2));
        f.push(WindowId(1));
        assert_eq!(f.depth(), 2);
        assert_eq!(f.current(), Some(WindowId(1)));
        f.pop();
        assert_eq!(f.current(), Some(WindowId(2)), "no stale duplicate underneath");
    }

    #[test]
    fn closing_a_window_removes_it_from_anywhere_in_the_stack() {
        let mut f = FocusStack::new();
        f.push(WindowId(1));
        f.push(WindowId(2));
        f.push(WindowId(3));
        f.remove(WindowId(2));
        assert_eq!(f.iter_top_down().collect::<Vec<_>>(), vec![WindowId(3), WindowId(1)]);
    }
}
