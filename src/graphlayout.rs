//! A small deterministic force-directed layout for the project-graph maps.
//!
//! Nodes repel each other and edges pull their endpoints together
//! (Fruchterman–Reingold); after a fixed number of cooling iterations the
//! positions settle into a readable "map". It is deterministic — a circular
//! start with an index-based offset, no RNG — so the same graph always lays out
//! the same way (and a re-layout after an edit doesn't jump around at random).

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
}

const SPACE: f32 = 1000.0;
const ITERATIONS: usize = 300;

/// Lay out `nodes` connected by `edges` (index pairs). Positions come back in
/// `[0,1]`; the caller scales them into the canvas.
pub fn layout(nodes: Vec<NodeInput>, edges: Vec<(usize, usize)>) -> Layout {
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
            let d = (disp[i].0 * disp[i].0 + disp[i].1 * disp[i].1).sqrt().max(0.01);
            let m = d.min(temp);
            pos[i].0 += disp[i].0 / d * m;
            pos[i].1 += disp[i].1 / d * m;
        }
        temp *= 0.97;
    }

    // Normalize the bounding box into the unit square.
    let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in &pos {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    let sx = (maxx - minx).max(0.01);
    let sy = (maxy - miny).max(0.01);

    let out_nodes = nodes
        .into_iter()
        .enumerate()
        .map(|(i, ni)| LNode {
            label: ni.label,
            file: ni.file,
            x: (pos[i].0 - minx) / sx,
            y: (pos[i].1 - miny) / sy,
            weight: ni.weight,
            cyclic: ni.cyclic,
        })
        .collect();

    Layout { nodes: out_nodes, edges }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ni(label: &str) -> NodeInput {
        NodeInput { label: label.into(), file: PathBuf::from(label), weight: 1.0, cyclic: false }
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
    fn handles_empty_and_single() {
        assert!(layout(Vec::new(), Vec::new()).nodes.is_empty());
        let one = layout(vec![ni("solo")], Vec::new());
        assert_eq!(one.nodes.len(), 1);
        assert_eq!((one.nodes[0].x, one.nodes[0].y), (0.5, 0.5));
    }
}
