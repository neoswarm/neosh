//! A tab belongs to a conversation, and the bar is one conversation's tabs.
//!
//! The invariant worth protecting is that *everything counted on the bar is counted in the visible
//! tabs*. A shell opened while reading one conversation is that conversation's, so switching has to
//! take it off the strip — and the moment it is off the strip, every key that walks the bar has a
//! second answer available to it and the wrong one is silent: `<C-w>n` steps into a conversation you
//! were not in, `<C-w>2` lands on somebody else's tab, and closing the last tab you can see puts you
//! somewhere you never asked to be.
//!
//! What the editor knows about a group is only that two of them can be equal. These use plain
//! strings for that reason: the host fills them with a `SessionId`, and nothing here should care.

use neosh_core::Editor;
use neosh_proto::{TabId, ViewId};

const V: ViewId = ViewId::LOCAL;

/// A terminal with its first tab filed under `a`, the way `settle_tab_groups` does.
fn setup() -> (Editor, TabId) {
    let mut e = Editor::new();
    // Settling the view is what any first question about its layout does, and it is what makes the
    // first tab exist at all — the editor has no idea which conversation a terminal opens in.
    e.active_pane(V);
    let first = e.active_tab_of(V).expect("a settled view has a tab");
    e.tab_set_group(V, first, Some("a".into()));
    (e, first)
}

fn bar(e: &Editor) -> Vec<TabId> {
    e.visible_tabs_of(V).iter().map(|t| t.id).collect()
}

#[test]
fn a_new_tab_joins_the_bar_it_was_opened_on() {
    let (mut e, first) = setup();
    let (second, _) = e.tab_new(V, None, true);
    assert_eq!(bar(&e), vec![first, second], "a tab opened here is this conversation's");
    assert_eq!(e.tabs_of(V)[1].group.as_deref(), Some("a"));
}

#[test]
fn switching_conversation_takes_the_other_tabs_off_the_bar() {
    let (mut e, first) = setup();
    let (shell, _) = e.tab_new(V, Some("shell".into()), false);
    assert_eq!(bar(&e), vec![first, shell]);

    // What `arrive_in` does: the tab you switched in is now about the conversation in it.
    e.tab_set_group(V, first, Some("b".into()));
    assert_eq!(bar(&e), vec![first], "the shell belongs to the conversation you left");

    // And going back brings it with you. Nothing was closed.
    e.tab_set_group(V, first, Some("a".into()));
    assert_eq!(bar(&e), vec![first, shell], "coming back finds it still there");
}

#[test]
fn stepping_stays_on_this_conversations_tabs() {
    let (mut e, first) = setup();
    let (mine, _) = e.tab_new(V, None, false);
    let (theirs, _) = e.tab_new(V, None, false);
    e.tab_set_group(V, theirs, Some("b".into()));

    assert_eq!(e.tab_step(V, 1), Some(mine));
    // Wrapping is over the bar, so the second step comes back round rather than landing in `b`.
    assert_eq!(e.tab_step(V, 1), Some(first));
    assert_eq!(e.tab_step(V, -1), Some(mine));
}

#[test]
fn the_bar_counts_its_own_tabs() {
    let (mut e, _) = setup();
    let (_, _) = e.tab_new(V, None, false);
    let (theirs, _) = e.tab_new(V, None, false);
    e.tab_set_group(V, theirs, Some("b".into()));
    assert_eq!(e.tab_count_of(V), 2, "two on this bar, three in the terminal");
    assert_eq!(e.tabs_of(V).len(), 3);
}

#[test]
fn a_conversations_last_tab_is_refused_even_with_tabs_open_elsewhere() {
    let (mut e, first) = setup();
    let (theirs, _) = e.tab_new(V, None, false);
    e.tab_set_group(V, theirs, Some("b".into()));

    assert!(!e.tab_close(V, first), "closing it would land you in another conversation's tabs");
    assert_eq!(bar(&e), vec![first]);
    // A tab on a bar nobody is looking at has no such problem.
    assert!(e.tab_close(V, theirs));
}

#[test]
fn closing_hands_the_keyboard_to_a_tab_on_the_same_bar() {
    let (mut e, first) = setup();
    // Interleaved on purpose: `b`'s tab sits between two of `a`'s, which is exactly the case an
    // index into the whole list gets wrong.
    let (theirs, _) = e.tab_new(V, None, false);
    e.tab_set_group(V, theirs, Some("b".into()));
    let (mine, _) = e.tab_new(V, None, true);

    assert!(e.tab_close(V, mine));
    assert_eq!(e.active_tab_of(V), Some(first), "back to the tab on the left of this bar");
    assert_eq!(bar(&e), vec![first]);
}

#[test]
fn a_drag_lands_where_the_bar_says_it_landed() {
    let (mut e, first) = setup();
    let (theirs, _) = e.tab_new(V, None, false);
    e.tab_set_group(V, theirs, Some("b".into()));
    let (second, _) = e.tab_new(V, None, false);
    let (third, _) = e.tab_new(V, None, false);
    assert_eq!(bar(&e), vec![first, second, third]);

    // One place right, over a hidden tab: a swap in the underlying list would jump it two.
    assert!(e.tab_move(V, first, 1));
    assert_eq!(bar(&e), vec![second, first, third]);

    assert!(e.tab_move(V, first, 2));
    assert_eq!(bar(&e), vec![second, third, first]);

    assert!(e.tab_move(V, first, 0));
    assert_eq!(bar(&e), vec![first, second, third]);

    // And the other conversation's tab is where it was through all of it.
    assert!(e.tabs_of(V).iter().any(|t| t.id == theirs));
}

#[test]
fn an_unfiled_tab_is_its_own_bar() {
    // What every tab was before groups existed, and what the editor's first tab is until the host
    // settles it: `None` is a group like any other rather than "on every bar".
    let mut e = Editor::new();
    e.active_pane(V);
    let first = e.active_tab_of(V).expect("a settled view has a tab");
    let (second, _) = e.tab_new(V, None, false);
    assert_eq!(bar(&e), vec![first, second]);

    e.tab_set_group(V, second, Some("a".into()));
    assert_eq!(bar(&e), vec![first], "filing one takes it off the unfiled bar");
}
