//! A small deterministic force-directed layout for the project-graph maps.
//!
//! Nodes repel each other and edges pull their endpoints together
//! (Fruchterman–Reingold); after a fixed number of cooling iterations the
//! positions settle into a readable "map". It is deterministic — a circular
//! start with an index-based offset, no RNG — so the same graph always lays out
//! the same way (and a re-layout after an edit doesn't jump around at random).

use std::collections::HashSet;
use std::f32::consts::TAU;
use std::path::PathBuf;

/// One node to place.
pub struct NodeInput {
    pub label: String,
    pub file: PathBuf,
    /// Relative importance (e.g. degree), drives the drawn radius.
    pub weight: f32,
    /// Part of a cycle — drawn highlighted.
    pub cyclic: bool,
}

/// Upper bound on nodes actually laid out. The layout is O(n²) per iteration and
/// runs on the UI thread, and a node-link map is unreadable past ~150 nodes
/// anyway; a bigger graph is reduced to its highest-degree nodes so opening the
/// map stays instant. [`Layout::total`] reports the pre-cap count.
pub const MAX_LAYOUT_NODES: usize = 160;

/// A placed node, position normalized to the unit square `[0,1] × [0,1]`.
#[derive(Debug, Clone)]
pub struct LNode {
    pub label: String,
    pub file: PathBuf,
    pub x: f32,
    pub y: f32,
    pub weight: f32,
    pub cyclic: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Layout {
    pub nodes: Vec<LNode>,
    pub edges: Vec<(usize, usize)>,
    /// Node count before any [`MAX_LAYOUT_NODES`] cap (so the UI can say
    /// "showing N of total").
    pub total: usize,
}

const SPACE: f32 = 1000.0;
const ITERATIONS: usize = 300;

/// Lay out `nodes` connected by `edges` (index pairs). Positions come back in
/// `[0,1]`; the caller scales them into the canvas.
pub fn layout(nodes: Vec<NodeInput>, edges: Vec<(usize, usize)>) -> Layout {
    let total = nodes.len();
    // Cap huge graphs to their highest-degree nodes so the O(n²) layout on the
    // UI thread stays cheap (and the map stays legible).
    let (nodes, mut edges) = if nodes.len() > MAX_LAYOUT_NODES {
        cap_by_weight(nodes, edges, MAX_LAYOUT_NODES)
    } else {
        (nodes, edges)
    };
    let n = nodes.len();
    if n == 0 {
        return Layout::default();
    }
    if n == 1 {
        let i = &nodes[0];
        return Layout {
            nodes: vec![LNode {
                label: i.label.clone(),
                file: i.file.clone(),
                x: 0.5,
                y: 0.5,
                weight: i.weight,
                cyclic: i.cyclic,
            }],
            edges: Vec::new(),
            total,
        };
    }

    // Ideal edge length for the available area.
    let k = (SPACE * SPACE / n as f32).sqrt();
    let center = SPACE / 2.0;

    // Deterministic start: a circle, with an index-based radius wobble so the
    // symmetry breaks the same way every run.
    let mut pos: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let a = TAU * (i as f32) / (n as f32);
            let r = SPACE * 0.35 + ((i * 137) % 97) as f32;
            (center + r * a.cos(), center + r * a.sin())
        })
        .collect();

    let mut temp = SPACE / 10.0;
    for _ in 0..ITERATIONS {
        let mut disp = vec![(0.0f32, 0.0f32); n];

        // Repulsion between every pair.
        for i in 0..n {
            for j in (i + 1)..n {
                let dx = pos[i].0 - pos[j].0;
                let dy = pos[i].1 - pos[j].1;
                let d = (dx * dx + dy * dy).sqrt().max(0.01);
                let f = k * k / d;
                let (ux, uy) = (dx / d * f, dy / d * f);
                disp[i].0 += ux;
                disp[i].1 += uy;
                disp[j].0 -= ux;
                disp[j].1 -= uy;
            }
        }

        // Attraction along edges.
        for &(a, b) in &edges {
            if a == b || a >= n || b >= n {
                continue;
            }
            let dx = pos[a].0 - pos[b].0;
            let dy = pos[a].1 - pos[b].1;
            let d = (dx * dx + dy * dy).sqrt().max(0.01);
            let f = d * d / k;
            let (ux, uy) = (dx / d * f, dy / d * f);
            disp[a].0 -= ux;
            disp[a].1 -= uy;
            disp[b].0 += ux;
            disp[b].1 += uy;
        }

        // Apply, capped by the cooling temperature, with a little gravity so
        // disconnected nodes don't drift off to infinity.
        for i in 0..n {
            disp[i].0 += (center - pos[i].0) * 0.02;
            disp[i].1 += (center - pos[i].1) * 0.02;
            let d = (disp[i].0 * disp[i].0 + disp[i].1 * disp[i].1)
                .sqrt()
                .max(0.01);
            let m = d.min(temp);
            pos[i].0 += disp[i].0 / d * m;
            pos[i].1 += disp[i].1 / d * m;
        }
        temp *= 0.97;
    }

    // Pack connected components. The force sim pushes components apart and lets
    // isolated nodes (e.g. a file nothing calls) drift far from the cluster,
    // which inflates the bounding box and squashes the real cluster into a
    // corner with dead space around it. Re-pack each component's box tightly
    // (shelf packing) while keeping its internal FR layout intact, so the map
    // fills the canvas evenly regardless of how many components there are.
    let comps = connected_components(n, &edges);
    if comps.len() > 1 {
        // Bounding box per component: (min x, min y, width, height).
        let boxes: Vec<(f32, f32, f32, f32)> = comps
            .iter()
            .map(|comp| {
                let (mut nx, mut ny, mut xx, mut xy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                for &i in comp {
                    nx = nx.min(pos[i].0);
                    ny = ny.min(pos[i].1);
                    xx = xx.max(pos[i].0);
                    xy = xy.max(pos[i].1);
                }
                (nx, ny, xx - nx, xy - ny)
            })
            .collect();
        // Gap between packed components, scaled to the largest so it reads
        // consistently (and gives lone nodes a sensible cell of their own).
        let pad = boxes
            .iter()
            .map(|b| b.2.max(b.3))
            .fold(0.0f32, f32::max)
            .max(1.0)
            * 0.22;
        // Shelf packing: tallest boxes first, wrapping to a roughly square area.
        let mut order: Vec<usize> = (0..comps.len()).collect();
        order.sort_by(|&a, &b| {
            (boxes[b].3)
                .partial_cmp(&boxes[a].3)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(comps[a][0].cmp(&comps[b][0]))
        });
        let total_area: f32 = boxes.iter().map(|b| (b.2 + pad) * (b.3 + pad)).sum();
        let row_w = total_area.sqrt() * 1.3;
        // First pass: assign each box to a shelf (row) and an x within it.
        let mut shelves: Vec<Vec<usize>> = vec![Vec::new()];
        let mut shelf_of = vec![0usize; comps.len()];
        let mut x_in_shelf = vec![0.0f32; comps.len()];
        let mut cx = 0.0f32;
        for &ci in &order {
            let bw = boxes[ci].2 + pad;
            if cx > 0.0 && cx + bw > row_w {
                shelves.push(Vec::new());
                cx = 0.0;
            }
            let s = shelves.len() - 1;
            x_in_shelf[ci] = cx;
            shelf_of[ci] = s;
            shelves[s].push(ci);
            cx += bw;
        }
        // Shelf heights and their vertical starts.
        let shelf_h: Vec<f32> = shelves
            .iter()
            .map(|sh| sh.iter().map(|&ci| boxes[ci].3 + pad).fold(0.0, f32::max))
            .collect();
        let mut shelf_top = vec![0.0f32; shelves.len()];
        for i in 1..shelves.len() {
            shelf_top[i] = shelf_top[i - 1] + shelf_h[i - 1];
        }
        // Second pass: translate each component into its cell, centered
        // vertically within the shelf so a short box (a lone node) sits beside
        // the tall cluster rather than pinned to the shelf's top edge.
        let mut offset = vec![(0.0f32, 0.0f32); comps.len()];
        for &ci in &order {
            let (minx, miny, _w, h) = boxes[ci];
            let s = shelf_of[ci];
            let oy = shelf_top[s] + (shelf_h[s] - (h + pad)) * 0.5;
            offset[ci] = (x_in_shelf[ci] + pad * 0.5 - minx, oy + pad * 0.5 - miny);
        }
        for (ci, comp) in comps.iter().enumerate() {
            for &i in comp {
                pos[i].0 += offset[ci].0;
                pos[i].1 += offset[ci].1;
            }
        }
    }

    // Normalize the bounding box into the unit square.
    let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in &pos {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    let spanx = maxx - minx;
    let spany = maxy - miny;

    // Normalize into the unit square; if every point collapsed onto one axis
    // (degenerate), center on that axis rather than piling into a corner.
    let norm = |v: f32, min: f32, span: f32| if span > 0.01 { (v - min) / span } else { 0.5 };
    let out_nodes = nodes
        .into_iter()
        .enumerate()
        .map(|(i, ni)| LNode {
            label: ni.label,
            file: ni.file,
            x: norm(pos[i].0, minx, spanx),
            y: norm(pos[i].1, miny, spany),
            weight: ni.weight,
            cyclic: ni.cyclic,
        })
        .collect();

    // Never hand back self- or out-of-range edges — a renderer that indexes
    // `nodes[a]`/`nodes[b]` unchecked would otherwise panic.
    edges.retain(|&(a, b)| a != b && a < n && b < n);
    Layout {
        nodes: out_nodes,
        edges,
        total,
    }
}

/// Keep the `cap` highest-weight nodes (ties broken by original order) and the
/// edges between survivors, remapping indices to the reduced set.
fn cap_by_weight(
    nodes: Vec<NodeInput>,
    edges: Vec<(usize, usize)>,
    cap: usize,
) -> (Vec<NodeInput>, Vec<(usize, usize)>) {
    let mut order: Vec<usize> = (0..nodes.len()).collect();
    order.sort_by(|&a, &b| {
        nodes[b]
            .weight
            .partial_cmp(&nodes[a].weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.cmp(&b))
    });
    let keep: HashSet<usize> = order.into_iter().take(cap).collect();

    let mut new_idx = vec![usize::MAX; nodes.len()];
    let mut new_nodes = Vec::with_capacity(cap);
    for (old, node) in nodes.into_iter().enumerate() {
        if keep.contains(&old) {
            new_idx[old] = new_nodes.len();
            new_nodes.push(node);
        }
    }
    let new_edges = edges
        .into_iter()
        .filter(|(a, b)| keep.contains(a) && keep.contains(b))
        .map(|(a, b)| (new_idx[a], new_idx[b]))
        .collect();
    (new_nodes, new_edges)
}

/// Connected components (undirected) via union-find, each a sorted list of node
/// indices. Deterministic: components are keyed by their root and returned in
/// ascending root order, with member indices ascending.
fn connected_components(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]]; // path halving
            x = parent[x];
        }
        x
    }
    let mut parent: Vec<usize> = (0..n).collect();
    for &(a, b) in edges {
        if a < n && b < n && a != b {
            let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
            if ra != rb {
                parent[ra] = rb;
            }
        }
    }
    let mut groups: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    groups.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ni(label: &str) -> NodeInput {
        NodeInput {
            label: label.into(),
            file: PathBuf::from(label),
            weight: 1.0,
            cyclic: false,
        }
    }

    #[test]
    fn positions_are_normalized_and_deterministic() {
        let make = || {
            layout(
                vec![ni("a"), ni("b"), ni("c"), ni("d")],
                vec![(0, 1), (1, 2), (2, 3)],
            )
        };
        let l1 = make();
        let l2 = make();
        assert_eq!(l1.nodes.len(), 4);
        for node in &l1.nodes {
            assert!((0.0..=1.0).contains(&node.x), "x out of range: {}", node.x);
            assert!((0.0..=1.0).contains(&node.y), "y out of range: {}", node.y);
        }
        // Deterministic: identical input lays out identically.
        for (p, q) in l1.nodes.iter().zip(&l2.nodes) {
            assert_eq!(p.x, q.x);
            assert_eq!(p.y, q.y);
        }
    }

    #[test]
    fn connected_nodes_end_up_closer_than_unconnected() {
        // A dumbbell: 0-1 connected, 2-3 connected, the two pairs unlinked.
        let l = layout(
            vec![ni("0"), ni("1"), ni("2"), ni("3")],
            vec![(0, 1), (2, 3)],
        );
        let dist = |a: usize, b: usize| {
            let (dx, dy) = (l.nodes[a].x - l.nodes[b].x, l.nodes[a].y - l.nodes[b].y);
            (dx * dx + dy * dy).sqrt()
        };
        // Each connected pair sits closer than the cross-pair spread.
        assert!(dist(0, 1) < dist(0, 2) || dist(0, 1) < dist(0, 3));
    }

    #[test]
    fn isolated_node_packs_near_the_cluster() {
        // A connected 4-cycle plus one isolated node. Component packing should
        // keep the lone node adjacent to the cluster rather than flinging it to a
        // far corner with dead space between (the old force-only behavior).
        let l = layout(
            vec![ni("a"), ni("b"), ni("c"), ni("d"), ni("solo")],
            vec![(0, 1), (1, 2), (2, 3), (3, 0)],
        );
        let cx = l.nodes[0..4].iter().map(|n| n.x).sum::<f32>() / 4.0;
        let cy = l.nodes[0..4].iter().map(|n| n.y).sum::<f32>() / 4.0;
        let solo = &l.nodes[4];
        let d = ((solo.x - cx).powi(2) + (solo.y - cy).powi(2)).sqrt();
        assert!(d < 0.7, "isolated node too far from cluster: {d}");
    }

    #[test]
    fn handles_empty_and_single() {
        assert!(layout(Vec::new(), Vec::new()).nodes.is_empty());
        let one = layout(vec![ni("solo")], Vec::new());
        assert_eq!(one.nodes.len(), 1);
        assert_eq!((one.nodes[0].x, one.nodes[0].y), (0.5, 0.5));
    }

    #[test]
    fn caps_huge_graphs_to_highest_degree_nodes() {
        let count = MAX_LAYOUT_NODES + 50;
        // Node i has weight i, so the highest-weight nodes are the last ones.
        let nodes: Vec<NodeInput> = (0..count)
            .map(|i| NodeInput {
                label: i.to_string(),
                file: PathBuf::from(i.to_string()),
                weight: i as f32,
                cyclic: false,
            })
            .collect();
        // An edge between the two highest-weight nodes must survive the cap.
        let edges = vec![(count - 1, count - 2), (0, 1)];
        let l = layout(nodes, edges);
        assert_eq!(l.nodes.len(), MAX_LAYOUT_NODES);
        assert_eq!(l.total, count);
        // The top node (weight count-1) is kept; the lowest (0, 1) are dropped,
        // so their edge is gone but the top pair's edge remains.
        let labels: HashSet<&str> = l.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains((count - 1).to_string().as_str()));
        assert!(!labels.contains("0"));
        assert_eq!(l.edges.len(), 1);
    }
}
