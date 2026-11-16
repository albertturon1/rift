//! When the focused window's border is drawn.
//!
//! The rule comes from AeroSpace's `borders-toggle.sh` and was first the `nav borders` helper: the border is hidden
//! when the focused window sits in a stack (accordion), or when fewer than `min_visible` windows are on screen.
//! Floating windows always count as visible; a stack shows only its selected member. Pure functions on a
//! [`ContainerTreeNode`], like `group_nav`, so the decision can be made in the same step as a focus move.

use rift_protocol::{ContainerNodeType, ContainerTreeNode, LayoutKind, WindowId};

/// What the decision needs from the layout engine.
pub struct BorderInput<'a> {
    pub tree: &'a ContainerTreeNode,
    pub floating: &'a [WindowId],
    pub focused: Option<WindowId>,
}

/// The two user-facing switches of the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BorderRule {
    pub hide_in_stack: bool,
    pub min_visible_windows: usize,
}

fn is_stack(node: &ContainerTreeNode) -> bool {
    matches!(node.layout_kind, Some(LayoutKind::HorizontalStack | LayoutKind::VerticalStack))
}

fn contains_selected(node: &ContainerTreeNode) -> bool {
    node.is_selected || node.children.iter().any(contains_selected)
}

/// Windows really on screen under `node`: a stack shows only its selected member (the first one when none is).
fn visible_in(node: &ContainerTreeNode) -> usize {
    if node.node_type == ContainerNodeType::Window {
        return 1;
    }
    if node.children.is_empty() {
        return 0;
    }
    if is_stack(node) {
        let shown = node.children.iter().find(|c| contains_selected(c)).unwrap_or(&node.children[0]);
        return visible_in(shown);
    }
    node.children.iter().map(visible_in).sum()
}

fn parent_of<'a>(root: &'a ContainerTreeNode, window: WindowId) -> Option<&'a ContainerTreeNode> {
    root.children.iter().find_map(|child| {
        if child.node_type == ContainerNodeType::Window && child.window_id == Some(window) {
            Some(root)
        } else {
            parent_of(child, window)
        }
    })
}

pub fn should_show(input: &BorderInput<'_>, rule: BorderRule) -> bool {
    let (Some(focused), tree) = (input.focused, input.tree) else {
        return false;
    };
    let visible = input.floating.len() + visible_in(tree);
    let enough = visible >= rule.min_visible_windows;
    if input.floating.contains(&focused) {
        return enough;
    }
    match parent_of(tree, focused) {
        None => false,
        Some(parent) if rule.hide_in_stack && is_stack(parent) => false,
        Some(_) => enough,
    }
}

#[cfg(test)]
mod tests {
    use rift_protocol::{ContainerNodeType, ContainerTreeNode, LayoutKind};

    use super::*;

    const RULE: BorderRule = BorderRule { hide_in_stack: true, min_visible_windows: 2 };

    fn wid(idx: u32) -> WindowId { WindowId { pid: 1, idx } }

    fn window(idx: u32, selected: bool) -> ContainerTreeNode {
        ContainerTreeNode {
            node_type: ContainerNodeType::Window,
            window_id: Some(wid(idx)),
            is_selected: selected,
            ..ContainerTreeNode::default()
        }
    }

    fn container(kind: LayoutKind, children: Vec<ContainerTreeNode>) -> ContainerTreeNode {
        ContainerTreeNode {
            node_type: ContainerNodeType::Container,
            layout_kind: Some(kind),
            children,
            ..ContainerTreeNode::default()
        }
    }

    fn show(tree: &ContainerTreeNode, floating: &[WindowId], focused: Option<u32>) -> bool {
        should_show(&BorderInput { tree, floating, focused: focused.map(wid) }, RULE)
    }

    #[test]
    fn two_tiled_windows_show_the_border_and_one_hides_it() {
        let two = container(LayoutKind::Horizontal, vec![window(1, true), window(2, false)]);
        assert!(show(&two, &[], Some(1)));
        let one = container(LayoutKind::Horizontal, vec![window(1, true)]);
        assert!(!show(&one, &[], Some(1)));
    }

    #[test]
    fn nothing_focused_hides_the_border() {
        let two = container(LayoutKind::Horizontal, vec![window(1, false), window(2, false)]);
        assert!(!show(&two, &[], None));
    }

    #[test]
    fn a_window_in_a_stack_hides_the_border_even_with_many_windows() {
        let stack = container(LayoutKind::HorizontalStack, vec![window(1, true), window(2, false), window(3, false)]);
        assert!(!show(&stack, &[], Some(1)));
        // The same three windows side by side show it.
        let row = container(LayoutKind::Horizontal, vec![window(1, true), window(2, false), window(3, false)]);
        assert!(show(&row, &[], Some(1)));
    }

    #[test]
    fn a_stack_counts_as_one_visible_window() {
        // [A [stack B C]]: A is plain, the stack shows one window, so two are visible and A keeps its border.
        let stack = container(LayoutKind::VerticalStack, vec![window(2, false), window(3, false)]);
        let tree = container(LayoutKind::Horizontal, vec![window(1, true), stack]);
        assert!(show(&tree, &[], Some(1)));
        // A stacked root with one plain window next to nothing else: still hidden (in a stack).
        let lonely = container(LayoutKind::Horizontal, vec![container(LayoutKind::HorizontalStack, vec![window(1, true), window(2, false)])]);
        assert!(!show(&lonely, &[], Some(1)));
    }

    #[test]
    fn leaving_a_stack_for_a_plain_slot_flips_the_decision() {
        // [A [stack B C]] with focus predicted on B (in the stack) hides, on A shows: the move case the old script
        // had to preset before focus moved.
        let stack = container(LayoutKind::HorizontalStack, vec![window(2, true), window(3, false)]);
        let tree = container(LayoutKind::Horizontal, vec![window(1, false), stack]);
        assert!(show(&tree, &[], Some(1)));
        assert!(!show(&tree, &[], Some(2)));
    }

    #[test]
    fn floating_windows_count_and_a_focused_floating_window_follows_the_count() {
        let one = container(LayoutKind::Horizontal, vec![window(1, true)]);
        assert!(!show(&one, &[], Some(1)));
        assert!(show(&one, &[wid(9)], Some(1)), "a floating window makes two visible");
        assert!(show(&one, &[wid(9)], Some(9)));
        let empty = container(LayoutKind::Horizontal, vec![]);
        assert!(!show(&empty, &[wid(9)], Some(9)), "a lone floating window hides it");
    }

    #[test]
    fn a_focused_window_missing_from_the_tree_hides_the_border() {
        let two = container(LayoutKind::Horizontal, vec![window(1, true), window(2, false)]);
        assert!(!show(&two, &[], Some(7)));
    }

    #[test]
    fn the_switches_change_the_rule() {
        let stack = container(LayoutKind::HorizontalStack, vec![window(1, true), window(2, false)]);
        let tree = container(LayoutKind::Horizontal, vec![stack, window(3, false)]);
        let input = BorderInput { tree: &tree, floating: &[], focused: Some(wid(1)) };
        assert!(!should_show(&input, RULE));
        assert!(should_show(&input, BorderRule { hide_in_stack: false, ..RULE }));
        assert!(should_show(&input, BorderRule { hide_in_stack: false, min_visible_windows: 1 }));
        let single = container(LayoutKind::Horizontal, vec![window(1, true)]);
        let input = BorderInput { tree: &single, floating: &[], focused: Some(wid(1)) };
        assert!(should_show(&input, BorderRule { hide_in_stack: true, min_visible_windows: 1 }));
    }

    /// Evaluates the rule on layout JSON saved from `rift-cli query layout`: `RIFT_LAYOUT_JSON=file`.
    #[test]
    #[ignore = "reads a layout file named by an environment variable"]
    fn live_layout_json() {
        let path = std::env::var("RIFT_LAYOUT_JSON").expect("set RIFT_LAYOUT_JSON");
        let state: rift_protocol::LayoutStateData =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let focused = state.selected_window;
        let show = should_show(
            &BorderInput { tree: &state.container_tree, floating: &state.floating_windows, focused },
            RULE,
        );
        println!("focused {focused:?} -> show {show}");
    }
}
