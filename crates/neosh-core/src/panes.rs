//! The pane tree: how one tab divides the main region, and every operation on it.
//!
//! Pure tree arithmetic over [`PaneNode`], with no ids allocated, no windows touched and no events
//! emitted — [`crate::Editor`] does all three around these. Kept apart because this is the part
//! that is worth being *certain* about: a split that nests when it should flatten, or a close that
//! leaves a one-child split behind, is not a crash but a layout that drifts one level deeper every
//! time you use it, and by then the tree that produced it is three keystrokes in the past.
//!
//! # Two invariants, maintained by every function here
//!
//! - **A split has at least two children.** One-child splits are collapsed into the child on the
//!   way out of every removal. Otherwise `<C-w>l` steps through levels with nothing in them, and a
//!   resize divides space that is not on screen.
//! - **A split is never a direct child of a split with the same `dir`.** Three panes in a row are
//!   one `Row` of three. Nested, the second boundary would move a different amount from the first
//!   for the same keypress, because it would be dividing a child rather than the region.
//!
//! Both are re-established by construction rather than checked afterwards, and
//! [`PaneNode::well_formed`] exists so the tests can say so.

use neosh_proto::{Direction, PaneChild, PaneId, PaneNode, WEIGHT};

/// The least weight a child may be reduced to.
///
/// A pane resized to zero is one you cannot see, cannot focus by pointing at it, and — since every
/// key that would grow it again is bound inside it — cannot get back. Ten per cent of an even share
/// is small enough to be a sliver and large enough to still be somewhere.
const MIN_WEIGHT: u16 = WEIGHT / 10;

/// Where a pane sits in a tree, as the child index at each level from the root.
///
/// An empty path is the root itself, which is a leaf only when the tab holds one pane.
type Path = Vec<usize>;

/// The child indices leading to `pane`, or nothing if it is not in this tree.
pub fn path(root: &PaneNode, pane: PaneId) -> Option<Path> {
    fn walk(node: &PaneNode, pane: PaneId, at: &mut Path) -> bool {
        match node {
            PaneNode::Leaf { pane: p } => *p == pane,
            PaneNode::Split { children, .. } => {
                for (i, c) in children.iter().enumerate() {
                    at.push(i);
                    if walk(&c.node, pane, at) {
                        return true;
                    }
                    at.pop();
                }
                false
            }
        }
    }
    let mut at = Vec::new();
    walk(root, pane, &mut at).then_some(at)
}

fn at<'a>(root: &'a PaneNode, path: &[usize]) -> Option<&'a PaneNode> {
    let mut node = root;
    for &i in path {
        let PaneNode::Split { children, .. } = node else { return None };
        node = &children.get(i)?.node;
    }
    Some(node)
}

fn at_mut<'a>(root: &'a mut PaneNode, path: &[usize]) -> Option<&'a mut PaneNode> {
    let mut node = root;
    for &i in path {
        let PaneNode::Split { children, .. } = node else { return None };
        node = &mut children.get_mut(i)?.node;
    }
    Some(node)
}

/// Divide `pane` in two, putting `new` on its `dir` side.
///
/// Flattens rather than nests when the parent already divides along this axis, which is what keeps
/// three side-by-side panes one `Row` of three. The new pane inherits half of `pane`'s weight, so
/// splitting a pane that somebody had already made wide does not quietly give the halves an even
/// share of the whole region.
///
/// Answers whether `pane` was there to split.
pub fn split(root: &mut PaneNode, pane: PaneId, dir: Direction, new: PaneId) -> bool {
    let Some(path) = path(root, pane) else { return false };
    let want = dir.split();

    // The parent divides along this axis already: this is a third pane in an existing row, not a
    // row inside a row.
    if let Some((&idx, parent_path)) = path.split_last() {
        if let Some(PaneNode::Split { dir: d, children }) = at_mut(root, parent_path) {
            if *d == want {
                let half = (children[idx].weight / 2).max(MIN_WEIGHT);
                let kept = children[idx].weight.saturating_sub(half).max(MIN_WEIGHT);
                children[idx].weight = kept;
                let put = if dir.forward() { idx + 1 } else { idx };
                children.insert(put, PaneChild { node: PaneNode::Leaf { pane: new }, weight: half });
                return true;
            }
        }
    }

    // Otherwise the leaf itself becomes a split of two.
    let Some(node) = at_mut(root, &path) else { return false };
    let mine = PaneChild::new(node.clone());
    let theirs = PaneChild::new(PaneNode::Leaf { pane: new });
    let children = if dir.forward() { vec![mine, theirs] } else { vec![theirs, mine] };
    *node = PaneNode::Split { dir: want, children };
    true
}

/// Take `pane` out of the tree, or answer `false` if it was the only one left.
///
/// The last pane of a tab is refused here rather than by the caller, because "a tab with no panes"
/// is a state the tree can represent and nothing downstream can draw — every consumer would need
/// the same guard, and the one that forgot it would be a tab that renders as nothing at all.
pub fn close(root: &mut PaneNode, pane: PaneId) -> bool {
    let Some(path) = path(root, pane) else { return false };
    let Some((&idx, parent_path)) = path.split_last() else {
        // The root is the leaf: nothing else is open.
        return false;
    };

    let Some(PaneNode::Split { children, .. }) = at_mut(root, parent_path) else { return false };
    let gone = children.remove(idx);
    // Its space goes back to the siblings, in proportion to what they already had — so closing the
    // narrow pane of three does not hand its column to whichever one happens to be first.
    let total: u32 = children.iter().map(|c| u32::from(c.weight)).sum();
    if total > 0 {
        let mut given = 0u32;
        for c in children.iter_mut() {
            let share = u32::from(gone.weight) * u32::from(c.weight) / total;
            c.weight = c.weight.saturating_add(share.min(u32::from(u16::MAX)) as u16);
            given += share;
        }
        // Integer division loses up to one unit per sibling, and losing any of it means the tab is
        // no longer fully divided — which nothing renders wrong today, but which drifts downwards
        // every time a pane is closed until an "even" split visibly is not one. The remainder goes
        // to the last sibling, arbitrarily and on purpose: it is at most a few units, and picking a
        // rule beats letting it evaporate.
        if let Some(last) = children.last_mut() {
            let lost = u32::from(gone.weight).saturating_sub(given);
            last.weight = last.weight.saturating_add(lost.min(u32::from(u16::MAX)) as u16);
        }
    }

    collapse(root, parent_path);
    true
}

/// Pull a one-child split up into its child, and merge a split into a parent of the same axis.
///
/// Run from the level a child was removed at, upwards: removing the second of two children makes a
/// one-child split, replacing it with its child may put a `Row` directly inside a `Row`, and
/// merging *that* may do it again one level up.
fn collapse(root: &mut PaneNode, mut path: &[usize]) {
    loop {
        let Some(node) = at_mut(root, path) else { return };
        let PaneNode::Split { children, .. } = node else { return };

        if children.len() == 1 {
            let only = children.remove(0);
            *node = only.node;
        }

        // Now that this level may have become a leaf or a differently-shaped split, see whether it
        // belongs to its parent's row.
        let Some((_, parent_path)) = path.split_last() else { return };
        let merged = merge_into_parent(root, parent_path);
        if !merged && at(root, path).is_some_and(|n| !matches!(n, PaneNode::Split { .. })) {
            // A leaf cannot collapse further, and nothing merged: the shape above is unchanged.
            return;
        }
        path = parent_path;
        if path.is_empty() {
            // The root still needs its own one-child check, and has no parent to merge into.
            let Some(PaneNode::Split { children, .. }) = at_mut(root, &[]) else { return };
            if children.len() == 1 {
                let only = children.remove(0);
                *root = only.node;
            }
            return;
        }
    }
}

/// Splice any child of this split that is itself a split of the same axis into it.
///
/// Answers whether anything moved. The spliced-in grandchildren divide the weight their parent
/// held, so flattening never changes what is on screen — only how many levels it took to say it.
fn merge_into_parent(root: &mut PaneNode, path: &[usize]) -> bool {
    let Some(PaneNode::Split { dir, children }) = at_mut(root, path) else { return false };
    let dir = *dir;
    let Some(idx) = children
        .iter()
        .position(|c| matches!(&c.node, PaneNode::Split { dir: d, .. } if *d == dir))
    else {
        return false;
    };

    let host = children.remove(idx);
    let PaneNode::Split { children: inner, .. } = host.node else { return false };
    let inner_total: u32 = inner.iter().map(|c| u32::from(c.weight)).sum::<u32>().max(1);
    let spliced: Vec<PaneChild> = inner
        .into_iter()
        .map(|c| {
            let w = u32::from(host.weight) * u32::from(c.weight) / inner_total;
            PaneChild { node: c.node, weight: (w as u16).max(MIN_WEIGHT) }
        })
        .collect();
    for (n, c) in spliced.into_iter().enumerate() {
        children.insert(idx + n, c);
    }
    true
}

/// The pane a move in `dir` lands on, or nothing at the edge of the layout.
///
/// Walks up to the first split dividing space along this axis that has a sibling on the far side,
/// then down the near edge of it. `prefer` is consulted on the way down — the panes this view has
/// been in most recently, newest first — so that leaving a pane and coming back lands where you
/// were rather than in whichever child happens to be first. Without it, `<C-w>l` then `<C-w>h`
/// is not a round trip, which is the thing about split navigation people notice immediately.
pub fn neighbour(
    root: &PaneNode,
    pane: PaneId,
    dir: Direction,
    prefer: &[PaneId],
) -> Option<PaneId> {
    let path = path(root, pane)?;
    let want = dir.split();

    for depth in (0..path.len()).rev() {
        let Some(PaneNode::Split { dir: d, children }) = at(root, &path[..depth]) else { continue };
        if *d != want {
            continue;
        }
        let idx = path[depth];
        let next = if dir.forward() {
            if idx + 1 >= children.len() {
                continue;
            }
            idx + 1
        } else {
            // Not `?`: being the first child of *this* split says nothing about the one above it,
            // and returning here would stop the walk at the first level that had no room.
            if idx == 0 {
                continue;
            }
            idx - 1
        };
        return Some(edge(&children[next].node, dir, prefer));
    }
    None
}

/// The pane you arrive at entering `node` while travelling in `dir`.
///
/// The near edge along the axis being crossed — going `Right`, the leftmost child of a `Row` — and
/// the most recently visited child across it, since a `Column` has no near edge with respect to a
/// horizontal move and any choice there is arbitrary unless it is remembered.
fn edge(node: &PaneNode, dir: Direction, prefer: &[PaneId]) -> PaneId {
    match node {
        PaneNode::Leaf { pane } => *pane,
        PaneNode::Split { dir: d, children } => {
            if *d == dir.split() {
                let near = if dir.forward() { 0 } else { children.len() - 1 };
                edge(&children[near].node, dir, prefer)
            } else {
                let recent = prefer.iter().find(|p| node.contains(**p)).copied();
                match recent {
                    Some(p) => p,
                    None => edge(&children[0].node, dir, prefer),
                }
            }
        }
    }
}

/// Move the boundary on `pane`'s `dir` side by `delta` weight units.
///
/// Growing means the neighbour on that side gives up exactly what this one gains, so the rest of
/// the layout does not move — which is the difference between resizing a pane and reflowing a tab.
/// A pane with no boundary on that side is a no-op rather than an error: the key is held down, and
/// a message for every press after the last one that fit is noise where nothing has gone wrong.
///
/// Answers whether anything moved.
pub fn resize(root: &mut PaneNode, pane: PaneId, dir: Direction, delta: i16) -> bool {
    let Some(path) = path(root, pane) else { return false };
    let want = dir.split();

    for depth in (0..path.len()).rev() {
        let Some(PaneNode::Split { dir: d, children }) = at_mut(root, &path[..depth]) else {
            continue;
        };
        if *d != want {
            continue;
        }
        let idx = path[depth];
        let other = if dir.forward() {
            if idx + 1 >= children.len() {
                continue;
            }
            idx + 1
        } else {
            if idx == 0 {
                continue;
            }
            idx - 1
        };

        // Clamped by what the *giver* can spare, in both directions: a grow that would push the
        // neighbour under the minimum takes only what is there, and a shrink is the same trade
        // read the other way round.
        let (mine, theirs) = (children[idx].weight, children[other].weight);
        let room = if delta >= 0 {
            i32::from(theirs) - i32::from(MIN_WEIGHT)
        } else {
            i32::from(mine) - i32::from(MIN_WEIGHT)
        }
        .max(0);
        let step = i32::from(delta).abs().min(room) * if delta >= 0 { 1 } else { -1 };
        if step == 0 {
            return false;
        }
        children[idx].weight = (i32::from(mine) + step) as u16;
        children[other].weight = (i32::from(theirs) - step) as u16;
        return true;
    }
    false
}

/// Exchange two panes' places, keeping the weights where they are.
///
/// The weights stay with the *slot* rather than travelling with the pane, because this is how a
/// pane is moved around a layout: swapping into the wide slot is how you make a pane wide, and a
/// swap that carried widths with it would leave the layout looking exactly as it did.
pub fn swap(root: &mut PaneNode, a: PaneId, b: PaneId) -> bool {
    if a == b {
        return false;
    }
    let (Some(pa), Some(pb)) = (path(root, a), path(root, b)) else { return false };
    let Some(na) = at_mut(root, &pa) else { return false };
    *na = PaneNode::Leaf { pane: b };
    let Some(nb) = at_mut(root, &pb) else { return false };
    *nb = PaneNode::Leaf { pane: a };
    true
}

/// Move a pane to the far edge of its tab, in `dir`.
///
/// Vim's `<C-w>H`/`J`/`K`/`L`, and the thing that was missing: you could move *between* panes and
/// not move *a* pane, so a layout that came out the wrong way round had to be closed and rebuilt.
///
/// The pane is taken out where it was — with all the collapsing that implies — and put back as the
/// first or last child of a split along `dir`'s axis, made at the root if there is not one already.
/// It arrives with an even share rather than the width it had: it is in a different place among
/// different neighbours, and carrying the old number over would make it the odd one out for a
/// reason nobody could see.
///
/// Answers `false` when there is nothing to do — one pane, or a pane already alone on that edge.
pub fn move_to_edge(root: &mut PaneNode, pane: PaneId, dir: Direction) -> bool {
    let all = root.panes();
    if all.len() < 2 || !all.contains(&pane) {
        return false;
    }
    // Already the whole of that edge: the root divides along this axis and this pane is the child
    // on the end. Rebuilding would be a no-op that still renumbered every weight.
    if let PaneNode::Split { dir: d, children } = root {
        if *d == dir.split() {
            let end = if dir.forward() { children.len() - 1 } else { 0 };
            if matches!(&children[end].node, PaneNode::Leaf { pane: p } if *p == pane) {
                return false;
            }
        }
    }

    if !close(root, pane) {
        return false;
    }
    let moved = PaneChild::new(PaneNode::Leaf { pane });
    match root {
        // The root already divides along this axis, so this is one more child on the end of it
        // rather than a new level wrapping what is there.
        PaneNode::Split { dir: d, children } if *d == dir.split() => {
            match dir.forward() {
                true => children.push(moved),
                false => children.insert(0, moved),
            }
        }
        other => {
            let rest = PaneChild::new(other.clone());
            let children = match dir.forward() { true => vec![rest, moved], false => vec![moved, rest] };
            *other = PaneNode::Split { dir: dir.split(), children };
        }
    }
    true
}

/// Give every child of every split an equal share.
pub fn equalize(node: &mut PaneNode) {
    if let PaneNode::Split { children, .. } = node {
        for c in children.iter_mut() {
            c.weight = WEIGHT;
            equalize(&mut c.node);
        }
    }
}

/// Whether this tree keeps both invariants. For tests, and for a debug assertion.
pub fn well_formed(node: &PaneNode) -> bool {
    match node {
        PaneNode::Leaf { .. } => true,
        PaneNode::Split { dir, children } => {
            children.len() >= 2
                && children.iter().all(|c| {
                    c.weight > 0
                        && !matches!(&c.node, PaneNode::Split { dir: d, .. } if d == dir)
                        && well_formed(&c.node)
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neosh_proto::SplitDir;

    fn p(n: u32) -> PaneId {
        PaneId(n)
    }

    fn leaf(n: u32) -> PaneNode {
        PaneNode::Leaf { pane: p(n) }
    }

    /// Build a tree by splitting, the way the keyboard does, and assert it stayed well formed.
    fn build(steps: &[(u32, Direction, u32)]) -> PaneNode {
        let mut root = leaf(1);
        for &(from, dir, new) in steps {
            assert!(split(&mut root, p(from), dir, p(new)), "split {from} -> {new}");
            assert!(well_formed(&root), "after splitting {from}: {root:?}");
        }
        root
    }

    #[test]
    fn splitting_a_lone_pane_makes_a_split_of_two() {
        let root = build(&[(1, Direction::Right, 2)]);
        assert_eq!(root.panes(), vec![p(1), p(2)]);
        assert!(matches!(root, PaneNode::Split { dir: SplitDir::Row, .. }));
    }

    #[test]
    fn splitting_left_puts_the_new_pane_first() {
        let root = build(&[(1, Direction::Left, 2)]);
        assert_eq!(root.panes(), vec![p(2), p(1)]);
    }

    #[test]
    fn splitting_up_puts_the_new_pane_above() {
        let root = build(&[(1, Direction::Up, 2)]);
        assert_eq!(root.panes(), vec![p(2), p(1)]);
        assert!(matches!(root, PaneNode::Split { dir: SplitDir::Column, .. }));
    }

    /// The flattening invariant: a third pane in a row joins the row rather than nesting in it.
    #[test]
    fn a_third_pane_on_one_axis_is_one_split_of_three() {
        let root = build(&[(1, Direction::Right, 2), (2, Direction::Right, 3)]);
        assert_eq!(root.panes(), vec![p(1), p(2), p(3)]);
        let PaneNode::Split { dir, children } = &root else { panic!("{root:?}") };
        assert_eq!(*dir, SplitDir::Row);
        assert_eq!(children.len(), 3, "three panes in a row are one row of three");
    }

    #[test]
    fn splitting_across_the_axis_nests() {
        let root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        assert_eq!(root.panes(), vec![p(1), p(2), p(3)]);
        let PaneNode::Split { dir, children } = &root else { panic!() };
        assert_eq!(*dir, SplitDir::Row);
        assert_eq!(children.len(), 2);
        assert!(matches!(&children[1].node, PaneNode::Split { dir: SplitDir::Column, .. }));
    }

    #[test]
    fn a_split_takes_half_of_what_the_pane_it_divided_had() {
        let mut root = build(&[(1, Direction::Right, 2)]);
        // Make pane 2 wide, then split it: the halves share *its* width, not the region's.
        resize(&mut root, p(2), Direction::Left, 60);
        let PaneNode::Split { children, .. } = &root else { panic!() };
        let (before_1, before_2) = (children[0].weight, children[1].weight);
        assert_eq!(before_1 + before_2, WEIGHT * 2);

        split(&mut root, p(2), Direction::Right, p(3));
        let PaneNode::Split { children, .. } = &root else { panic!() };
        assert_eq!(children[0].weight, before_1, "the pane nobody touched keeps its width");
        assert_eq!(children[1].weight + children[2].weight, before_2);
    }

    #[test]
    fn closing_the_last_pane_is_refused() {
        let mut root = leaf(1);
        assert!(!close(&mut root, p(1)));
        assert_eq!(root.panes(), vec![p(1)]);
    }

    #[test]
    fn closing_one_of_two_collapses_the_split_away() {
        let mut root = build(&[(1, Direction::Right, 2)]);
        assert!(close(&mut root, p(2)));
        assert_eq!(root, leaf(1), "a split with one child is that child");
        assert!(well_formed(&root));
    }

    #[test]
    fn closing_gives_its_space_to_its_siblings() {
        let mut root = build(&[(1, Direction::Right, 2), (2, Direction::Right, 3)]);
        close(&mut root, p(2));
        let PaneNode::Split { children, .. } = &root else { panic!() };
        assert_eq!(children.len(), 2);
        assert_eq!(
            children[0].weight + children[1].weight,
            WEIGHT * 2,
            "the region is still fully divided"
        );
    }

    /// The collapse has to keep flattening upwards, or a close leaves a row inside a row.
    #[test]
    fn collapsing_a_nested_split_merges_it_into_its_grandparent() {
        // 1 | (2 above 3) — then close 2, leaving 3 alone inside a Column inside a Row.
        let mut root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        assert!(close(&mut root, p(2)));
        assert!(well_formed(&root), "a Row must not contain a Row: {root:?}");
        assert_eq!(root.panes(), vec![p(1), p(3)]);
        let PaneNode::Split { dir, children } = &root else { panic!() };
        assert_eq!(*dir, SplitDir::Row);
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn closing_down_to_one_pane_leaves_a_bare_leaf() {
        let mut root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        close(&mut root, p(2));
        close(&mut root, p(3));
        assert_eq!(root, leaf(1));
        assert!(!close(&mut root, p(1)));
    }

    #[test]
    fn neighbours_step_across_the_axis_they_are_asked_about() {
        let root = build(&[(1, Direction::Right, 2), (2, Direction::Right, 3)]);
        assert_eq!(neighbour(&root, p(1), Direction::Right, &[]), Some(p(2)));
        assert_eq!(neighbour(&root, p(2), Direction::Right, &[]), Some(p(3)));
        assert_eq!(neighbour(&root, p(2), Direction::Left, &[]), Some(p(1)));
    }

    /// No wrap-around: the edge of the layout is where a move stops.
    #[test]
    fn there_is_no_neighbour_past_the_edge() {
        let root = build(&[(1, Direction::Right, 2)]);
        assert_eq!(neighbour(&root, p(1), Direction::Left, &[]), None);
        assert_eq!(neighbour(&root, p(2), Direction::Right, &[]), None);
        assert_eq!(neighbour(&root, p(1), Direction::Up, &[]), None);
        assert_eq!(neighbour(&root, p(1), Direction::Down, &[]), None);
    }

    /// Being the first child of one split says nothing about the split above it — the walk has to
    /// keep going up rather than give up at the first level with no room.
    #[test]
    fn a_move_escapes_the_split_it_is_at_the_edge_of() {
        // 1 | (2 above 3): from 2, going left leaves the Column entirely and finds 1.
        let root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        assert_eq!(neighbour(&root, p(2), Direction::Left, &[]), Some(p(1)));
        assert_eq!(neighbour(&root, p(3), Direction::Left, &[]), Some(p(1)));
        assert_eq!(neighbour(&root, p(2), Direction::Down, &[]), Some(p(3)));
    }

    #[test]
    fn entering_a_split_lands_on_its_near_edge() {
        // (1 | 2) with 2 split into 2 | 3 — arriving from the left lands on 2, not 3.
        let root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        assert_eq!(neighbour(&root, p(1), Direction::Right, &[]), Some(p(2)));
    }

    #[test]
    fn a_move_across_the_axis_prefers_where_you_were_last() {
        let root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        // Nothing remembered: the first child.
        assert_eq!(neighbour(&root, p(1), Direction::Right, &[]), Some(p(2)));
        // Having been in 3 most recently, going right comes back to 3.
        assert_eq!(neighbour(&root, p(1), Direction::Right, &[p(3), p(1)]), Some(p(3)));
    }

    #[test]
    fn moving_out_and_back_is_a_round_trip() {
        let root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        let out = neighbour(&root, p(3), Direction::Left, &[]).unwrap();
        assert_eq!(out, p(1));
        assert_eq!(neighbour(&root, out, Direction::Right, &[p(3), p(1)]), Some(p(3)));
    }

    #[test]
    fn resizing_moves_weight_from_the_neighbour_on_that_side() {
        let mut root = build(&[(1, Direction::Right, 2)]);
        assert!(resize(&mut root, p(1), Direction::Right, 20));
        let PaneNode::Split { children, .. } = &root else { panic!() };
        assert_eq!(children[0].weight, WEIGHT + 20);
        assert_eq!(children[1].weight, WEIGHT - 20);
    }

    #[test]
    fn resizing_at_the_edge_does_nothing() {
        let mut root = build(&[(1, Direction::Right, 2)]);
        assert!(!resize(&mut root, p(1), Direction::Left, 20));
        assert!(!resize(&mut root, p(1), Direction::Up, 20));
        let PaneNode::Split { children, .. } = &root else { panic!() };
        assert_eq!(children[0].weight, WEIGHT);
    }

    /// A pane must never be resized out of existence — every key that would grow it again is
    /// inside it.
    #[test]
    fn a_pane_cannot_be_shrunk_to_nothing() {
        let mut root = build(&[(1, Direction::Right, 2)]);
        for _ in 0..50 {
            resize(&mut root, p(1), Direction::Right, 50);
        }
        let PaneNode::Split { children, .. } = &root else { panic!() };
        assert!(children[1].weight >= MIN_WEIGHT, "{:?}", children[1].weight);
        assert_eq!(children[0].weight + children[1].weight, WEIGHT * 2, "space is conserved");
    }

    #[test]
    fn swapping_exchanges_places_and_leaves_the_shape_alone() {
        let mut root = build(&[(1, Direction::Right, 2), (2, Direction::Right, 3)]);
        resize(&mut root, p(1), Direction::Right, 40);
        let PaneNode::Split { children, .. } = &root else { panic!() };
        let widths: Vec<u16> = children.iter().map(|c| c.weight).collect();

        assert!(swap(&mut root, p(1), p(3)));
        assert_eq!(root.panes(), vec![p(3), p(2), p(1)]);
        let PaneNode::Split { children, .. } = &root else { panic!() };
        assert_eq!(
            children.iter().map(|c| c.weight).collect::<Vec<_>>(),
            widths,
            "the widths belong to the slots, so swapping into the wide one makes you wide"
        );
    }

    #[test]
    fn a_pane_moves_to_the_far_edge() {
        // 1 | (2 above 3) — send 3 to the far left and it becomes a column of the top-level row.
        let mut root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        assert!(move_to_edge(&mut root, p(3), Direction::Left));
        assert!(well_formed(&root), "{root:?}");
        assert_eq!(root.panes(), vec![p(3), p(1), p(2)]);
        let PaneNode::Split { dir, children } = &root else { panic!("{root:?}") };
        assert_eq!(*dir, SplitDir::Row);
        assert_eq!(children.len(), 3, "and the column it left collapsed away");
    }

    #[test]
    fn moving_across_the_axis_wraps_the_layout() {
        let mut root = build(&[(1, Direction::Right, 2)]);
        assert!(move_to_edge(&mut root, p(2), Direction::Up));
        assert!(well_formed(&root), "{root:?}");
        assert_eq!(root.panes(), vec![p(2), p(1)]);
        let PaneNode::Split { dir, children } = &root else { panic!() };
        assert_eq!(*dir, SplitDir::Column, "a row became a column with the moved pane on top");
        assert_eq!(children.len(), 2);
    }

    /// Already there is a no-op, not a rebuild that renumbers every weight.
    #[test]
    fn moving_a_pane_that_is_already_on_that_edge_does_nothing() {
        let mut root = build(&[(1, Direction::Right, 2), (2, Direction::Right, 3)]);
        let before = root.clone();
        assert!(!move_to_edge(&mut root, p(1), Direction::Left));
        assert!(!move_to_edge(&mut root, p(3), Direction::Right));
        assert_eq!(root, before);
    }

    #[test]
    fn a_lone_pane_has_nowhere_to_move_to() {
        let mut root = leaf(1);
        assert!(!move_to_edge(&mut root, p(1), Direction::Left));
        assert_eq!(root, leaf(1));
    }

    /// Moving must never lose a pane, whatever the shape or the direction.
    #[test]
    fn moving_conserves_every_pane() {
        let dirs = [Direction::Left, Direction::Right, Direction::Up, Direction::Down];
        for (i, dir) in dirs.iter().enumerate() {
            let mut root = build(&[
                (1, Direction::Right, 2),
                (2, Direction::Down, 3),
                (1, Direction::Down, 4),
                (3, Direction::Right, 5),
            ]);
            let before = root.panes().len();
            let target = root.panes()[i % 5];
            move_to_edge(&mut root, target, *dir);
            assert!(well_formed(&root), "moving {target:?} {dir:?}: {root:?}");
            assert_eq!(root.panes().len(), before, "no pane was lost moving {target:?} {dir:?}");
            let mut got = root.panes();
            got.sort_by_key(|p| p.0);
            assert_eq!(got, vec![p(1), p(2), p(3), p(4), p(5)]);
        }
    }

    #[test]
    fn equalizing_gives_every_split_an_even_share() {
        let mut root = build(&[(1, Direction::Right, 2), (2, Direction::Down, 3)]);
        resize(&mut root, p(1), Direction::Right, 50);
        resize(&mut root, p(2), Direction::Down, 30);
        equalize(&mut root);
        fn even(n: &PaneNode) -> bool {
            match n {
                PaneNode::Leaf { .. } => true,
                PaneNode::Split { children, .. } => {
                    children.iter().all(|c| c.weight == WEIGHT && even(&c.node))
                }
            }
        }
        assert!(even(&root));
    }

    /// Every op on a pane that is not in the tree has to be a refusal rather than a panic or a
    /// silent write to whatever was at that path.
    #[test]
    fn operations_on_an_unknown_pane_are_refused() {
        let mut root = build(&[(1, Direction::Right, 2)]);
        let before = root.clone();
        assert!(!split(&mut root, p(99), Direction::Right, p(3)));
        assert!(!close(&mut root, p(99)));
        assert!(!resize(&mut root, p(99), Direction::Right, 10));
        assert!(!swap(&mut root, p(99), p(1)));
        assert!(!swap(&mut root, p(1), p(1)));
        assert_eq!(neighbour(&root, p(99), Direction::Right, &[]), None);
        assert_eq!(root, before);
    }

    /// The shape that broke every earlier attempt: split, split across, close the middle, and the
    /// tree has to come back to a flat row rather than keeping an empty level.
    #[test]
    fn a_long_run_of_splits_and_closes_stays_well_formed() {
        let mut root = leaf(1);
        let mut next = 2;
        let dirs =
            [Direction::Right, Direction::Down, Direction::Left, Direction::Up, Direction::Right];
        // Grow to a dozen panes, then take them all away in the order they arrived.
        for i in 0..12 {
            let panes = root.panes();
            let from = panes[i % panes.len()];
            assert!(split(&mut root, from, dirs[i % dirs.len()], p(next)));
            assert!(well_formed(&root), "after split {next}: {root:?}");
            next += 1;
        }
        assert_eq!(root.panes().len(), 13);
        for id in 2..next {
            assert!(close(&mut root, p(id)), "closing {id}");
            assert!(well_formed(&root), "after closing {id}: {root:?}");
        }
        assert_eq!(root, leaf(1));
    }

    /// Every pane must be reachable from every other by directional moves alone, or a split is a
    /// place the keyboard cannot get to.
    #[test]
    fn every_pane_is_reachable_by_direction() {
        let root = build(&[
            (1, Direction::Right, 2),
            (2, Direction::Down, 3),
            (1, Direction::Down, 4),
            (3, Direction::Right, 5),
        ]);
        let all = root.panes();
        for &from in &all {
            let mut seen = vec![from];
            let mut queue = vec![from];
            while let Some(at) = queue.pop() {
                for dir in
                    [Direction::Left, Direction::Right, Direction::Up, Direction::Down]
                {
                    if let Some(n) = neighbour(&root, at, dir, &[]) {
                        if !seen.contains(&n) {
                            seen.push(n);
                            queue.push(n);
                        }
                    }
                }
            }
            assert_eq!(seen.len(), all.len(), "from {from:?} reached only {seen:?} of {all:?}");
        }
    }
}
