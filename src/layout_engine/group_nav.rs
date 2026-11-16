//! Group-aware focus moves, decided on the layout topology.
//!
//! `next_sibling`, `next_slot` and `move_focus_in_group` replace the `nav` helper program the dotfiles used to run
//! through `rift-cli`. A group is a container that is not the root; a slot is a child of the root. Everything here is
//! a pure function on a [`ContainerTreeNode`]: the engine builds the tree, drops the windows that cannot take focus
//! with [`prune`], asks for a target and then focuses it. Nothing here talks to the window server.

use rift_protocol::{ContainerNodeType, ContainerTreeNode, Direction, LayoutKind, WindowId};

/// What a directional press does inside a group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupMove {
    /// Not ours: Rift's own focus move handles it.
    Native,
    /// The press ends here and focus stays where it is.
    Stay,
    /// Wrap to the far end of the group.
    Wrap(WindowId),
}

/// Where a next/previous slot press lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotTarget {
    /// A child of the root, by node id. The caller picks the member of a group it remembers.
    Slot(u64),
    Floating(WindowId),
}

fn is_window(node: &ContainerTreeNode) -> bool { node.node_type == ContainerNodeType::Window }

fn is_vertical(node: &ContainerTreeNode) -> bool {
    matches!(node.layout_kind, Some(LayoutKind::Vertical | LayoutKind::VerticalStack))
}

/// The nodes from the root down to the selected node, or empty when nothing is selected.
fn path_to_selected(root: &ContainerTreeNode) -> Vec<&ContainerTreeNode> {
    fn walk<'a>(node: &'a ContainerTreeNode, trail: &mut Vec<&'a ContainerTreeNode>) -> bool {
        trail.push(node);
        if node.is_selected || node.children.iter().any(|child| walk(child, trail)) {
            return true;
        }
        trail.pop();
        false
    }
    let mut trail = Vec::new();
    walk(root, &mut trail);
    trail
}

/// The first window under `node`, or `None` when it holds none.
pub fn first_window(node: &ContainerTreeNode) -> Option<WindowId> {
    if is_window(node) {
        return node.window_id;
    }
    node.children.iter().find_map(first_window)
}

fn position(children: &[&ContainerTreeNode], id: u64) -> Option<usize> {
    children.iter().position(|child| child.node_id == id)
}

/// The selected window, if the selection is on one.
pub fn selected_window(root: &ContainerTreeNode) -> Option<WindowId> {
    path_to_selected(root).last().filter(|node| is_window(node)).and_then(|node| node.window_id)
}

/// Whether `window` is a window node somewhere in the tree.
pub fn contains_window(root: &ContainerTreeNode, window: WindowId) -> bool {
    (is_window(root) && root.window_id == Some(window)) || root.children.iter().any(|c| contains_window(c, window))
}

/// Whether the root is an accordion: every child of it is then a slot, a window or a group alike.
pub fn root_is_stack(root: &ContainerTreeNode) -> bool {
    matches!(root.layout_kind, Some(LayoutKind::HorizontalStack | LayoutKind::VerticalStack))
}

/// Whether the workspace has a group at its top level. Without one there is nothing for a slot move to cross: the
/// windows are plain siblings and belong to `next_sibling`.
pub fn has_group(root: &ContainerTreeNode) -> bool { root.children.iter().any(|c| !is_window(c)) }

/// Decide a left/right/up/down press. Outside a group nothing moves; inside a group a move along the group's own
/// direction stays in the group (side-by-side groups wrap, stacked groups stop at the edge).
pub fn decide_move(root: &ContainerTreeNode, direction: Direction) -> GroupMove {
    let path = path_to_selected(root);
    if path.len() < 2 {
        return GroupMove::Native;
    }
    if path.len() == 2 {
        return GroupMove::Stay;
    }
    let sideways = matches!(direction, Direction::Left | Direction::Right);
    let forward = matches!(direction, Direction::Right | Direction::Down);
    let group = path[path.len() - 2];
    if is_vertical(group) == sideways {
        return GroupMove::Native;
    }
    let siblings: Vec<&ContainerTreeNode> = group.children.iter().collect();
    let Some(index) = position(&siblings, path[path.len() - 1].node_id) else {
        return GroupMove::Native;
    };
    let at_edge = if forward { index + 1 == siblings.len() } else { index == 0 };
    if !at_edge {
        return GroupMove::Native;
    }
    if !sideways {
        return GroupMove::Stay;
    }
    let far = if forward { siblings.first() } else { siblings.last() };
    far.and_then(|node| first_window(node)).map_or(GroupMove::Stay, GroupMove::Wrap)
}

/// The window a next-sibling or previous-sibling press lands on, wrapping at both ends. Inside a group it is the
/// neighbouring member. For a window outside any group it is the next plain top-level window: this move never
/// enters a group from outside. Under an accordion root the top-level windows are slots, which `slot_target` walks, so
/// there it does nothing outside a group. `None` when there is no other candidate.
pub fn sibling_target(root: &ContainerTreeNode, forward: bool) -> Option<WindowId> {
    let path = path_to_selected(root);
    if path.len() < 2 {
        return None;
    }
    let (container, own) = (path[path.len() - 2], path[path.len() - 1]);
    let inside_group = path.len() > 2;
    if !inside_group && root_is_stack(root) {
        return None;
    }
    let peers: Vec<&ContainerTreeNode> =
        container.children.iter().filter(|c| inside_group || is_window(c)).collect();
    let count = peers.len();
    let index = position(&peers, own.node_id)?;
    if count < 2 {
        return None;
    }
    let next = if forward { (index + 1) % count } else { (index + count - 1) % count };
    first_window(peers[next])
}

/// The item a next-slot or previous-slot press lands on. Items are the top-level slots in tree order, then the
/// floating windows, wrapping from the last item to the first. `focus_is_floating` says which kind of item holds
/// focus; the tree's own selection does not follow focus to a floating window.
pub fn slot_target(
    root: &ContainerTreeNode,
    floating: &[WindowId],
    focus: Option<WindowId>,
    forward: bool,
) -> Option<SlotTarget> {
    let tiled = root.children.len();
    let total = tiled + floating.len();
    if total < 2 || (!has_group(root) && !root_is_stack(root) && floating.is_empty()) {
        return None;
    }
    let floating_index = focus.and_then(|window| floating.iter().position(|f| *f == window));
    let current = match floating_index {
        Some(index) => tiled + index,
        None => {
            let path = path_to_selected(root);
            if path.len() < 2 {
                return None;
            }
            root.children.iter().position(|c| c.node_id == path[1].node_id)?
        }
    };
    let target = if forward { (current + 1) % total } else { (current + total - 1) % total };
    Some(if target >= tiled {
        SlotTarget::Floating(floating[target - tiled])
    } else {
        SlotTarget::Slot(root.children[target].node_id)
    })
}

/// A copy of `node` without the windows `usable` rejects and without groups that end up empty. `None` when nothing of
/// `node` is left.
fn without_unusable(
    node: &ContainerTreeNode,
    usable: &dyn Fn(WindowId) -> bool,
) -> Option<ContainerTreeNode> {
    if is_window(node) && node.window_id.is_some_and(|window| !usable(window)) {
        return None;
    }
    let children: Vec<ContainerTreeNode> =
        node.children.iter().filter_map(|child| without_unusable(child, usable)).collect();
    if !node.children.is_empty() && children.is_empty() {
        return None;
    }
    Some(ContainerTreeNode { children, ..node.clone() })
}

fn clear_selection(node: &mut ContainerTreeNode) {
    node.is_selected = false;
    node.children.iter_mut().for_each(clear_selection);
}

fn select_window_node(node: &mut ContainerTreeNode, window: WindowId) -> bool {
    if is_window(node) && node.window_id == Some(window) {
        node.is_selected = true;
        return true;
    }
    node.children.iter_mut().any(|child| select_window_node(child, window))
}

/// The tree without the windows that cannot take focus (minimized windows, windows of hidden apps). When the
/// selection was on a dropped window, or on a container, it moves to `focus`.
pub fn prune(
    root: &ContainerTreeNode,
    focus: Option<WindowId>,
    usable: &dyn Fn(WindowId) -> bool,
) -> ContainerTreeNode {
    let mut pruned = without_unusable(root, usable)
        .unwrap_or_else(|| ContainerTreeNode { children: Vec::new(), ..root.clone() });
    if selected_window(&pruned).is_none()
        && let Some(window) = focus
    {
        clear_selection(&mut pruned);
        select_window_node(&mut pruned, window);
    }
    pruned
}

/// Fullscreen flags on the selected node: (full, within gaps).
pub fn selected_fullscreen(root: &ContainerTreeNode) -> (bool, bool) {
    path_to_selected(root)
        .last()
        .map_or((false, false), |node| (node.is_fullscreen, node.is_fullscreen_within_gaps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wid(idx: u32) -> WindowId { WindowId { pid: 1, idx } }

    struct Ids(u64);
    impl Ids {
        fn next(&mut self) -> u64 {
            self.0 += 1;
            self.0
        }
    }

    fn window(ids: &mut Ids, idx: u32) -> ContainerTreeNode {
        ContainerTreeNode {
            node_id: ids.next(),
            node_type: ContainerNodeType::Window,
            window_id: Some(wid(idx)),
            ..Default::default()
        }
    }

    fn container(ids: &mut Ids, kind: LayoutKind, children: Vec<ContainerTreeNode>) -> ContainerTreeNode {
        ContainerTreeNode {
            node_id: ids.next(),
            node_type: ContainerNodeType::Container,
            layout_kind: Some(kind),
            children,
            ..Default::default()
        }
    }

    fn select(node: &mut ContainerTreeNode, idx: u32) {
        clear_selection(node);
        assert!(select_window_node(node, wid(idx)), "window {idx} is not in the tree");
    }

    use LayoutKind::{Horizontal, Vertical};

    /// `[A [X Y] B]` with A=1, X=2, Y=3, B=4.
    fn group_between(kind: LayoutKind) -> ContainerTreeNode {
        let mut ids = Ids(0);
        let (a, x, y, b) = (window(&mut ids, 1), window(&mut ids, 2), window(&mut ids, 3), window(&mut ids, 4));
        let group = container(&mut ids, kind, vec![x, y]);
        container(&mut ids, Horizontal, vec![a, group, b])
    }

    #[test]
    fn side_by_side_group_wraps_at_both_edges_and_walks_inside() {
        let mut tree = group_between(Horizontal);
        select(&mut tree, 2);
        assert_eq!(decide_move(&tree, Direction::Right), GroupMove::Native);
        assert_eq!(decide_move(&tree, Direction::Left), GroupMove::Wrap(wid(3)));
        let mut tree = group_between(Horizontal);
        select(&mut tree, 3);
        assert_eq!(decide_move(&tree, Direction::Right), GroupMove::Wrap(wid(2)));
        assert_eq!(decide_move(&tree, Direction::Left), GroupMove::Native);
    }

    #[test]
    fn stacked_group_stops_at_its_edge_along_its_own_direction() {
        let mut tree = group_between(Vertical);
        select(&mut tree, 2);
        assert_eq!(decide_move(&tree, Direction::Up), GroupMove::Stay);
        assert_eq!(decide_move(&tree, Direction::Down), GroupMove::Native);
        select(&mut tree, 3);
        assert_eq!(decide_move(&tree, Direction::Down), GroupMove::Stay);
        // Across the group's axis Rift's own move runs.
        assert_eq!(decide_move(&tree, Direction::Left), GroupMove::Native);
    }

    #[test]
    fn outside_a_group_a_directional_press_does_nothing() {
        let mut tree = group_between(Horizontal);
        select(&mut tree, 1);
        for direction in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            assert_eq!(decide_move(&tree, direction), GroupMove::Stay);
        }
    }

    #[test]
    fn sibling_moves_inside_a_group_and_never_enters_one() {
        let mut tree = group_between(Horizontal);
        select(&mut tree, 2);
        assert_eq!(sibling_target(&tree, true), Some(wid(3)));
        assert_eq!(sibling_target(&tree, false), Some(wid(3)));
        select(&mut tree, 3);
        assert_eq!(sibling_target(&tree, true), Some(wid(2)));
        // A and B are the plain windows: A -> B and back, the group is skipped.
        let mut tree = group_between(Horizontal);
        select(&mut tree, 1);
        assert_eq!(sibling_target(&tree, true), Some(wid(4)));
        assert_eq!(sibling_target(&tree, false), Some(wid(4)));
    }

    #[test]
    fn sibling_has_nowhere_to_go_next_to_a_group_with_one_plain_window() {
        let mut ids = Ids(0);
        let (a, x, y) = (window(&mut ids, 1), window(&mut ids, 2), window(&mut ids, 3));
        let group = container(&mut ids, Horizontal, vec![x, y]);
        let mut tree = container(&mut ids, Horizontal, vec![a, group]);
        select(&mut tree, 1);
        assert_eq!(sibling_target(&tree, true), None);
    }

    #[test]
    fn sibling_walks_a_group_of_three_with_wrap() {
        let mut ids = Ids(0);
        let members: Vec<_> = (1..=3).map(|i| window(&mut ids, i)).collect();
        let group = container(&mut ids, Horizontal, members);
        let mut tree = container(&mut ids, Horizontal, vec![group]);
        select(&mut tree, 1);
        assert_eq!(sibling_target(&tree, true), Some(wid(2)));
        select(&mut tree, 3);
        assert_eq!(sibling_target(&tree, true), Some(wid(1)));
        select(&mut tree, 1);
        assert_eq!(sibling_target(&tree, false), Some(wid(3)));
    }

    #[test]
    fn slot_moves_wrap_over_groups_and_do_nothing_without_one() {
        let mut tree = group_between(Horizontal);
        let slots: Vec<u64> = tree.children.iter().map(|c| c.node_id).collect();
        select(&mut tree, 4);
        assert_eq!(slot_target(&tree, &[], Some(wid(4)), true), Some(SlotTarget::Slot(slots[0])));
        select(&mut tree, 1);
        assert_eq!(slot_target(&tree, &[], Some(wid(1)), false), Some(SlotTarget::Slot(slots[2])));
        select(&mut tree, 2);
        assert_eq!(slot_target(&tree, &[], Some(wid(2)), true), Some(SlotTarget::Slot(slots[2])));

        let mut ids = Ids(0);
        let (a, b) = (window(&mut ids, 1), window(&mut ids, 2));
        let mut flat = container(&mut ids, Horizontal, vec![a, b]);
        select(&mut flat, 1);
        assert_eq!(slot_target(&flat, &[], Some(wid(1)), true), None);
    }

    #[test]
    fn slot_moves_visit_floating_windows_after_the_slots() {
        let mut ids = Ids(0);
        let (a, b) = (window(&mut ids, 1), window(&mut ids, 2));
        let mut flat = container(&mut ids, Horizontal, vec![a, b]);
        let slots: Vec<u64> = flat.children.iter().map(|c| c.node_id).collect();
        let floating = [wid(9)];
        select(&mut flat, 2);
        assert_eq!(slot_target(&flat, &floating, Some(wid(2)), true), Some(SlotTarget::Floating(wid(9))));
        // From the floating window the cycle wraps to the first slot and back to it when going backwards.
        assert_eq!(slot_target(&flat, &floating, Some(wid(9)), true), Some(SlotTarget::Slot(slots[0])));
        assert_eq!(slot_target(&flat, &floating, Some(wid(9)), false), Some(SlotTarget::Slot(slots[1])));
    }

    #[test]
    fn prune_drops_unusable_windows_and_empty_groups_and_moves_the_selection() {
        let mut tree = group_between(Horizontal);
        select(&mut tree, 2);
        // X (2) and Y (3) unusable: the group disappears; the selection falls back to the focused window.
        let usable = |w: WindowId| w != wid(2) && w != wid(3);
        let pruned = prune(&tree, Some(wid(4)), &usable);
        assert_eq!(pruned.children.len(), 2);
        assert!(!has_group(&pruned));
        assert_eq!(selected_window(&pruned), Some(wid(4)));
        // One member left: the group stays and Y is alone in it.
        let usable = |w: WindowId| w != wid(2);
        let pruned = prune(&tree, Some(wid(4)), &usable);
        assert!(has_group(&pruned));
        assert_eq!(pruned.children[1].children.len(), 1);
    }

    #[test]
    fn a_group_with_one_usable_member_wraps_onto_itself() {
        let mut tree = group_between(Horizontal);
        select(&mut tree, 3);
        let pruned = prune(&tree, Some(wid(3)), &|w| w != wid(2));
        assert_eq!(decide_move(&pruned, Direction::Right), GroupMove::Wrap(wid(3)));
        assert_eq!(decide_move(&pruned, Direction::Left), GroupMove::Wrap(wid(3)));
    }

    #[test]
    fn sibling_skips_unusable_windows() {
        let mut ids = Ids(0);
        let windows: Vec<_> = (1..=4).map(|i| window(&mut ids, i)).collect();
        let mut tree = container(&mut ids, Horizontal, windows);
        select(&mut tree, 2);
        let pruned = prune(&tree, Some(wid(2)), &|w| w != wid(3));
        assert_eq!(sibling_target(&pruned, true), Some(wid(4)));
        assert_eq!(sibling_target(&pruned, false), Some(wid(1)));
    }

    #[test]
    fn fullscreen_flags_come_from_the_selected_node() {
        let mut tree = group_between(Horizontal);
        select(&mut tree, 1);
        assert_eq!(selected_fullscreen(&tree), (false, false));
        tree.children[0].is_fullscreen = true;
        assert_eq!(selected_fullscreen(&tree), (true, false));
        tree.children[0].is_fullscreen = false;
        tree.children[0].is_fullscreen_within_gaps = true;
        assert_eq!(selected_fullscreen(&tree), (false, true));
    }

    /// A flat accordion `[A B C]` with A=1, B=2, C=3: the root is a stack and every child is a slot.
    fn flat_stack() -> ContainerTreeNode {
        let mut ids = Ids(0);
        let (a, b, c) = (window(&mut ids, 1), window(&mut ids, 2), window(&mut ids, 3));
        container(&mut ids, LayoutKind::HorizontalStack, vec![a, b, c])
    }

    #[test]
    fn slot_moves_walk_a_flat_stack_even_without_a_group() {
        let mut tree = flat_stack();
        select(&mut tree, 1);
        let slot_of = |tree: &ContainerTreeNode, target: Option<SlotTarget>| match target {
            Some(SlotTarget::Slot(id)) => tree.children.iter().find(|c| c.node_id == id).and_then(first_window),
            _ => None,
        };
        let next = slot_of(&tree, slot_target(&tree, &[], Some(wid(1)), true));
        assert_eq!(next, Some(wid(2)), "next slot of an accordion goes to its next window");
        select(&mut tree, 3);
        let wrap = slot_of(&tree, slot_target(&tree, &[], Some(wid(3)), true));
        assert_eq!(wrap, Some(wid(1)), "and wraps at the end");
        // A flat tiled row has no slots to cross: the windows are plain siblings.
        let mut row = flat_stack();
        row.layout_kind = Some(Horizontal);
        select(&mut row, 1);
        assert_eq!(slot_target(&row, &[], Some(wid(1)), true), None);
    }

    #[test]
    fn next_sibling_leaves_the_slots_of_an_accordion_to_next_slot() {
        let mut tree = flat_stack();
        select(&mut tree, 1);
        assert_eq!(sibling_target(&tree, true), None, "slots of an accordion are moved with next_slot only");
        // Inside a group of an accordion it still walks the members.
        let mut ids = Ids(0);
        let (a, x, y) = (window(&mut ids, 1), window(&mut ids, 2), window(&mut ids, 3));
        let group = container(&mut ids, Horizontal, vec![x, y]);
        let mut with_group = container(&mut ids, LayoutKind::HorizontalStack, vec![a, group]);
        select(&mut with_group, 2);
        assert_eq!(sibling_target(&with_group, true), Some(wid(3)));
    }
}
