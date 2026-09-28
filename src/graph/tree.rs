//! The lazily-expanded tree arena shared by the call hierarchy
//! ([`crate::callgraph::CallTree`]) and the import tree
//! ([`crate::imports::ImportTree`]): nodes live in one `Vec`, refer to each
//! other by index, and are walked in display order without recursion.

/// What the shared walks need from a node.
pub trait TreeNode {
    fn parent(&self) -> Option<usize>;
    /// `None` until the node's children have been loaded.
    fn children(&self) -> Option<&[usize]>;
    fn expanded(&self) -> bool;
}

/// Node ids in display order: depth-first from `roots`, descending only into
/// expanded nodes. Iterative, so a deep chain cannot overflow the stack; ids
/// out of range (never produced by the arenas) are skipped, not panicked on.
pub fn visible<N: TreeNode>(nodes: &[N], roots: &[usize]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
    while let Some(id) = stack.pop() {
        let Some(n) = nodes.get(id) else { continue };
        out.push(id);
        if n.expanded()
            && let Some(children) = n.children()
        {
            stack.extend(children.iter().rev());
        }
    }
    out
}

/// Whether `start` or any of its ancestors satisfies `pred` — how both trees
/// spot a node that repeats one on its own path (a cycle).
pub fn any_on_path<N: TreeNode>(
    nodes: &[N],
    mut start: Option<usize>,
    pred: impl Fn(&N) -> bool,
) -> bool {
    // Bounded by the arena size, so a corrupted parent link cannot loop.
    let mut steps = 0;
    while let Some(id) = start {
        let Some(n) = nodes.get(id) else { return false };
        if pred(n) {
            return true;
        }
        steps += 1;
        if steps > nodes.len() {
            return false;
        }
        start = n.parent();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    struct N {
        parent: Option<usize>,
        children: Option<Vec<usize>>,
        expanded: bool,
    }

    impl TreeNode for N {
        fn parent(&self) -> Option<usize> {
            self.parent
        }
        fn children(&self) -> Option<&[usize]> {
            self.children.as_deref()
        }
        fn expanded(&self) -> bool {
            self.expanded
        }
    }

    fn n(parent: Option<usize>, children: Option<Vec<usize>>, expanded: bool) -> N {
        N {
            parent,
            children,
            expanded,
        }
    }

    #[test]
    fn visible_is_depth_first_through_expanded_nodes_only() {
        // 0 ─┬─ 1 ── 3
        //    └─ 2 ── 4   (2 collapsed)
        let nodes = vec![
            n(None, Some(vec![1, 2]), true),
            n(Some(0), Some(vec![3]), true),
            n(Some(0), Some(vec![4]), false),
            n(Some(1), None, false),
            n(Some(2), None, false),
        ];
        assert_eq!(visible(&nodes, &[0]), [0, 1, 3, 2]);
        assert!(any_on_path(&nodes, Some(3), |x| x.children.as_deref()
            == Some(&[1, 2][..])));
        assert!(!any_on_path(&nodes, Some(4), |x| x.children.as_deref() == Some(&[3][..])));
    }

    /// A chain far deeper than any recursion limit walks fine.
    #[test]
    fn a_deep_chain_does_not_overflow() {
        let depth: usize = 200_000;
        let nodes: Vec<N> = (0..depth)
            .map(|i| {
                let child = (i + 1 < depth).then(|| vec![i + 1]);
                n(i.checked_sub(1), child, true)
            })
            .collect();
        assert_eq!(visible(&nodes, &[0]).len(), depth);
        assert!(any_on_path(&nodes, Some(depth - 1), |x| x.parent.is_none()));
    }
}
