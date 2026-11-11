//! A small deterministic force-directed layout for the project-graph maps.
//!
//! Nodes repel each other and edges pull their endpoints together
//! (Fruchterman–Reingold); after a fixed number of cooling iterations the
//! positions settle into a readable "map". It is deterministic — a circular
//! start with an index-based offset, no RNG, and the edges sorted before use
//! (callers often collect them from a `HashSet`, whose iteration order would
//! otherwise change the float summation order run to run) — so the same graph
//! always lays out the same way, and a re-layout after an edit doesn't jump
//! around at random.
//!
//! The live 3D simulation ([`fr_step3`], [`cool`]) is frame-rate independent:
//! damping and cooling are defined per unit of time, not per call.

use std::collections::HashSet;
use std::f32::consts::TAU;
use std::path::PathBuf;

/// One node to place.
pub struct NodeInput {
    pub label: String,
    pub file: PathBuf,
    /// 1-based line the node opens at (1 for a whole file).
    pub line: usize,
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
    /// 1-based line the node opens at (1 for a whole file).
    pub line: usize,
    pub x: f32,
    pub y: f32,
    pub weight: f32,
    pub cyclic: bool,
    /// Depth in the directed dependency hierarchy, normalized 0 (a root / entry
    /// point that nothing imports) … 1 (deepest). Drives the node's paleness in
    /// the map — deeper in the tree renders paler.
    pub depth: f32,
}

#[derive(Debug, Default, Clone)]
pub struct Layout {
    pub nodes: Vec<LNode>,
    pub edges: Vec<(usize, usize)>,
    /// Node count before any [`MAX_LAYOUT_NODES`] cap (so the UI can say
    /// "showing N of total").
    pub total: usize,
}

/// Labels that name different places are told apart: a file's by as many
/// of the folders over it as it takes (`app/mod.rs`, `ui/mod.rs`), anything
/// else's by its file after it (`new · lib.rs`, then `new · app/lib.rs`).
/// A map of a crate had a dozen nodes called `mod.rs`.
pub fn distinguish_labels(nodes: &mut [NodeInput]) {
    let tail = |file: &std::path::Path, parts: usize| -> String {
        let names: Vec<String> = file
            .components()
            .rev()
            .take(parts)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        names.into_iter().rev().collect::<Vec<_>>().join("/")
    };
    let mut by_label: std::collections::HashMap<String, Vec<usize>> = Default::default();
    for (i, n) in nodes.iter().enumerate() {
        by_label.entry(n.label.clone()).or_default().push(i);
    }
    for (label, same) in by_label {
        let files: HashSet<&PathBuf> = same.iter().map(|&i| &nodes[i].file).collect();
        if same.len() < 2 || files.len() < 2 {
            continue;
        }
        let files_named = same.iter().all(|&i| {
            nodes[i].file.file_name().map(|f| f.to_string_lossy()) == Some(label.as_str().into())
        });
        let deepest = same
            .iter()
            .map(|&i| nodes[i].file.components().count())
            .max()
            .unwrap_or(1);
        // Each as few folders as tell it from the others — or all of them.
        let named = |i: usize, parts: usize| -> String {
            if files_named {
                tail(&nodes[i].file, parts + 1)
            } else {
                format!("{label} ({})", tail(&nodes[i].file, parts))
            }
        };
        let mut names: Vec<Option<String>> = vec![None; same.len()];
        for parts in 1..=deepest {
            let at: Vec<String> = same.iter().map(|&i| named(i, parts)).collect();
            for (k, name) in at.iter().enumerate() {
                if names[k].is_none() && at.iter().filter(|n| *n == name).count() == 1 {
                    names[k] = Some(name.clone());
                }
            }
        }
        let names: Vec<String> = same
            .iter()
            .enumerate()
            .map(|(k, &i)| names[k].take().unwrap_or_else(|| named(i, deepest)))
            .collect();
        for (&i, name) in same.iter().zip(names) {
            nodes[i].label = name;
        }
    }
}

const SPACE: f32 = 1000.0;
const ITERATIONS: usize = 300;

/// Lay out `nodes` connected by `edges` (index pairs). Positions come back in
/// `[0,1]`; the caller scales them into the canvas.
pub fn layout(nodes: Vec<NodeInput>, mut edges: Vec<(usize, usize)>) -> Layout {
    let total = nodes.len();
    // Canonical edge order: the force sums below are float additions, so the
    // order edges arrive in changes the result. Duplicates would also double
    // an edge's pull.
    edges.sort_unstable();
    edges.dedup();
    // Cap huge graphs to their highest-degree nodes so the O(n²) layout on the
    // UI thread stays cheap (and the map stays legible).
    let (mut nodes, mut edges) = if nodes.len() > MAX_LAYOUT_NODES {
        cap_by_weight(nodes, edges, MAX_LAYOUT_NODES)
    } else {
        (nodes, edges)
    };
    distinguish_labels(&mut nodes);
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
                line: i.line,
                x: 0.5,
                y: 0.5,
                weight: i.weight,
                cyclic: i.cyclic,
                depth: 0.0,
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
    // Hierarchy depth over the (capped) directed graph, for the paleness cue.
    let depths = hierarchy_depth(n, &edges);
    let out_nodes = nodes
        .into_iter()
        .enumerate()
        .map(|(i, ni)| LNode {
            label: ni.label,
            file: ni.file,
            line: ni.line,
            x: norm(pos[i].0, minx, spanx),
            y: norm(pos[i].1, miny, spany),
            weight: ni.weight,
            cyclic: ni.cyclic,
            depth: depths[i],
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

/// A point/vector in the live 3D simulation's world space.
pub type V3 = [f32; 3];

/// Velocity damping of the live simulation: the fraction of velocity kept
/// after one REFERENCE step of [`REFERENCE_DT`] simulated seconds. The step
/// scales it to the actual `dt` (`DAMPING^(dt / REFERENCE_DT)`), so a frame
/// twice as long damps as much as two short ones — with a fixed per-call
/// factor, a 120 Hz display damped (and settled) twice as fast in wall time
/// as a 60 Hz one, and a stutter barely damped at all.
pub const DAMPING: f32 = 0.82;

/// The per-step duration [`DAMPING`] and [`COOLING`] were tuned at, in
/// simulated seconds: the canvas scales its 60 Hz frame time by 4.
pub const REFERENCE_DT: f32 = 4.0 / 60.0;

/// Cooling of the live simulation's `alpha` per [`REFERENCE_DT`] of simulated
/// time; see [`cool`].
pub const COOLING: f32 = 0.985;

/// Decay `alpha` for a step of `dt` simulated seconds — frame-rate
/// independent: `COOLING^(dt / REFERENCE_DT)`. The canvas used to multiply by
/// 0.985 per FRAME, so the graph froze twice as fast at 120 Hz and a dropped
/// frame cooled it by one frame's worth only.
pub fn cool(alpha: f32, dt: f32) -> f32 {
    (alpha * COOLING.powf(dt.max(0.0) / REFERENCE_DT)).max(0.0)
}

/// One Fruchterman–Reingold step for the *live* 3D graph animation, with
/// momentum.
///
/// `pos`/`vel` are 3D world-space arrays; `k` is the ideal edge length (use
/// [`ideal_k`]). Repulsion pushes every pair apart, edges pull their endpoints
/// together, and a weak pull toward the origin keeps disconnected pieces from
/// drifting away. Forces scale by `alpha` (a cooling factor the caller decays
/// toward 0 with [`cool`]), integrate into velocity with damping, then apply.
/// `dt` is the step's simulated duration: both the integration and the
/// damping ([`DAMPING`]) scale with it, so the motion does not depend on the
/// frame rate. A `pinned` node (being dragged) is held fixed so the rest
/// reacts around it. Returns the total kinetic energy, so the caller can stop
/// ticking once it settles.
pub fn fr_step3(
    pos: &mut [V3],
    vel: &mut [V3],
    edges: &[(usize, usize)],
    pinned: Option<usize>,
    k: f32,
    alpha: f32,
    dt: f32,
) -> f32 {
    // In steps no longer than `MAX_STEP`: a frame of a 60 Hz display is
    // stepped as two, of a 120 Hz one as one, so either moves the graph
    // alike — one long step of forces this stiff overshot where two short
    // ones did not.
    let steps = (dt.max(0.0) / MAX_STEP).ceil().max(1.0);
    let mut energy = 0.0;
    for _ in 0..steps as usize {
        energy = step_with(pos, vel, edges, pinned, k, alpha, dt / steps, &FORCES);
    }
    energy
}

/// The longest step of simulated time [`fr_step3`] takes at once: half a
/// [`REFERENCE_DT`], a 120 Hz frame's.
const MAX_STEP: f32 = REFERENCE_DT / 2.0;

/// The strengths of the live simulation's forces, each a multiple of the
/// ideal edge length `k`'s.
#[derive(Debug, Clone, Copy)]
struct Forces {
    /// Repulsion between every pair, `repel · k² / d` — a little more than
    /// Fruchterman–Reingold's `k² / d`, for an open shape rather than a knot.
    repel: f32,
    /// An edge's pull, `spring · d² / k` — softer than their `d² / k`, so
    /// densely linked files do not collapse into one another.
    spring: f32,
    /// The pull toward the centre, `gravity · k · d`. The canvas fits the
    /// graph's radius to its frame, so how much room nodes have on screen is
    /// how far apart they are against how far the farthest drift: a pull too
    /// weak let loosely linked files float far out, the fit shrank to hold
    /// them, and every cluster was drawn as a crowd of overlapping discs.
    gravity: f32,
}

const FORCES: Forces = Forces {
    repel: 1.8,
    spring: 0.45,
    gravity: 0.012,
};

#[allow(clippy::too_many_arguments)]
fn step_with(
    pos: &mut [V3],
    vel: &mut [V3],
    edges: &[(usize, usize)],
    pinned: Option<usize>,
    k: f32,
    alpha: f32,
    dt: f32,
    forces: &Forces,
) -> f32 {
    let n = pos.len();
    if n == 0 {
        return 0.0;
    }
    let mut disp = vec![[0.0f32; 3]; n];

    // Repulsion between every pair (O(n²), fine within MAX_LAYOUT_NODES).
    for i in 0..n {
        for j in (i + 1)..n {
            let d = [
                pos[i][0] - pos[j][0],
                pos[i][1] - pos[j][1],
                pos[i][2] - pos[j][2],
            ];
            let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(0.01);
            let f = forces.repel * k * k / dist;
            for c in 0..3 {
                let u = d[c] / dist * f;
                disp[i][c] += u;
                disp[j][c] -= u;
            }
        }
    }
    // Attraction along edges.
    for &(a, b) in edges {
        if a == b || a >= n || b >= n {
            continue;
        }
        let d = [
            pos[a][0] - pos[b][0],
            pos[a][1] - pos[b][1],
            pos[a][2] - pos[b][2],
        ];
        let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(0.01);
        let f = forces.spring * dist * dist / k;
        for c in 0..3 {
            let u = d[c] / dist * f;
            disp[a][c] -= u;
            disp[b][c] += u;
        }
    }

    // Integrate: gravity toward origin, cooled by alpha, velocity with damping,
    // speed clamped so a close pair can't explode. The pinned node stays put.
    let max_speed = k * 1.5;
    let damping = DAMPING.powf(dt.max(0.0) / REFERENCE_DT);
    let mut energy = 0.0;
    for i in 0..n {
        if Some(i) == pinned {
            vel[i] = [0.0; 3];
            continue;
        }
        for c in 0..3 {
            // Centering gravity: keeps loose pieces from drifting off, so the
            // fit leaves the rest room (see `Forces::gravity`).
            disp[i][c] += -pos[i][c] * forces.gravity * k;
            vel[i][c] = (vel[i][c] + disp[i][c] * alpha * dt) * damping;
        }
        let sp = (vel[i][0].powi(2) + vel[i][1].powi(2) + vel[i][2].powi(2)).sqrt();
        if sp > max_speed {
            for v in &mut vel[i] {
                *v *= max_speed / sp;
            }
        }
        for c in 0..3 {
            pos[i][c] += vel[i][c] * dt;
        }
        energy += vel[i][0].powi(2) + vel[i][1].powi(2) + vel[i][2].powi(2);
    }
    energy
}

/// Ideal edge length for `n` nodes in a world of side [`WORLD`], matching the
/// spacing the static [`layout`] settles to.
pub fn ideal_k(n: usize) -> f32 {
    (WORLD * WORLD / (n.max(1) as f32)).sqrt()
}

/// Side length of the live simulation's world space.
pub const WORLD: f32 = 1000.0;

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

/// Depth of each node in the *directed* hierarchy: the BFS distance (in edges)
/// from the roots — nodes nothing points at, i.e. entry points with in-degree 0.
/// An `(a, b)` edge means `a` depends on `b`, so `b` is one level deeper than
/// `a`. A component that is one big cycle (no in-degree-0 node) seeds its first
/// unvisited node as a pseudo-root so it still gets a depth. Returned normalized
/// to `0..1` (0 = root, 1 = deepest). Deterministic.
pub fn hierarchy_depth(n: usize, edges: &[(usize, usize)]) -> Vec<f32> {
    use std::collections::VecDeque;
    let mut indeg = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in edges {
        if a < n && b < n && a != b {
            indeg[b] += 1;
            adj[a].push(b);
        }
    }
    let mut depth = vec![u32::MAX; n];
    let mut q: VecDeque<usize> = VecDeque::new();
    for i in 0..n {
        if indeg[i] == 0 {
            depth[i] = 0;
            q.push_back(i);
        }
    }
    let mut start = 0;
    loop {
        while let Some(u) = q.pop_front() {
            let du = depth[u];
            for &v in &adj[u] {
                if depth[v] == u32::MAX {
                    depth[v] = du + 1;
                    q.push_back(v);
                }
            }
        }
        // Seed the next unvisited node (root-less cyclic components).
        while start < n && depth[start] != u32::MAX {
            start += 1;
        }
        if start >= n {
            break;
        }
        depth[start] = 0;
        q.push_back(start);
    }
    let maxd = depth.iter().copied().max().unwrap_or(0).max(1) as f32;
    depth.iter().map(|&d| d as f32 / maxd).collect()
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

    /// Nodes of one name in different places are told apart, by as little
    /// as it takes.
    #[test]
    fn labels_of_different_places_are_told_apart() {
        let node = |label: &str, file: &str| NodeInput {
            label: label.into(),
            file: PathBuf::from(file),
            line: 1,
            weight: 1.0,
            cyclic: false,
        };
        let mut nodes = vec![
            node("mod.rs", "/p/src/app/mod.rs"),
            node("mod.rs", "/p/src/ui/mod.rs"),
            node("mod.rs", "/p/crates/x/src/ui/mod.rs"),
            node("main.rs", "/p/src/main.rs"),
            node("new", "/p/src/a.rs"),
            node("new", "/p/src/b.rs"),
        ];
        distinguish_labels(&mut nodes);
        let labels: Vec<&str> = nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "app/mod.rs",
                "p/src/ui/mod.rs",
                "x/src/ui/mod.rs",
                "main.rs",
                "new (a.rs)",
                "new (b.rs)"
            ]
        );
    }

    fn ni(label: &str) -> NodeInput {
        NodeInput {
            label: label.into(),
            file: PathBuf::from(label),
            line: 1,
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
                line: 1,
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

    /// The same graph with its edges in a different order (as a `HashSet`
    /// hands them over) and with a duplicate must lay out bit-identically.
    #[test]
    fn layout_is_independent_of_edge_order() {
        let nodes = || (0..9).map(|i| ni(&i.to_string())).collect::<Vec<_>>();
        let edges = vec![
            (0, 1),
            (1, 2),
            (2, 0),
            (3, 4),
            (4, 5),
            (5, 6),
            (6, 3),
            (7, 8),
            (1, 5),
        ];
        let mut shuffled = edges.clone();
        shuffled.reverse();
        shuffled.swap(1, 6);
        shuffled.push((2, 0)); // a duplicate must not double the pull
        let a = layout(nodes(), edges);
        let b = layout(nodes(), shuffled);
        for (p, q) in a.nodes.iter().zip(&b.nodes) {
            assert_eq!((p.x, p.y), (q.x, q.y), "node {} moved", p.label);
        }
        assert_eq!(a.edges, b.edges);
    }

    /// Damping and cooling are per unit of time: a quarter second simulated
    /// as 15 frames or as 30 ends in (nearly) the same state. With the old
    /// per-call damping the 120 Hz run lost ~60% more energy and travelled a
    /// third less in the same simulated time.
    #[test]
    fn live_simulation_is_frame_rate_independent() {
        let n = 8;
        let edges: Vec<(usize, usize)> = (0..n).map(|i| (i, (i + 1) % n)).collect();
        let start: Vec<V3> = (0..n)
            .map(|i| {
                let a = TAU * i as f32 / n as f32;
                let r = 300.0 + ((i * 71) % 53) as f32;
                [r * a.cos(), r * a.sin(), ((i * 53) % 41) as f32 - 20.0]
            })
            .collect();
        let k = ideal_k(n);
        // (kinetic energy at the end, total distance travelled, final alpha)
        let run = |hz: usize| {
            let mut pos = start.clone();
            let mut vel = vec![[0.0f32; 3]; n];
            let mut alpha = 1.0f32;
            let mut energy = 0.0;
            let mut travelled = 0.0f32;
            // The canvas simulates 4x its frame time (see REFERENCE_DT).
            let dt = 4.0 / hz as f32;
            for _ in 0..hz / 4 {
                let before = pos.clone();
                energy = fr_step3(&mut pos, &mut vel, &edges, None, k, alpha, dt);
                alpha = cool(alpha, dt);
                travelled += pos
                    .iter()
                    .zip(&before)
                    .map(|(p, q)| {
                        ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2))
                            .sqrt()
                    })
                    .sum::<f32>();
            }
            (energy, travelled, alpha)
        };
        let (e60, t60, a60) = run(60);
        let (e120, t120, a120) = run(120);
        let close = |a: f32, b: f32| (a - b).abs() / a.abs().max(b.abs()) < 0.05;
        assert!(close(e60, e120), "energy after 0.25 s: {e60} vs {e120}");
        assert!(close(t60, t120), "distance travelled: {t60} vs {t120}");
        assert!((a60 - a120).abs() < 1e-4, "alpha {a60} vs {a120}");
        assert!(
            (a60 - COOLING.powi(15)).abs() < 1e-4,
            "cooling per reference step"
        );
    }

    #[test]
    fn cooling_composes_over_time() {
        // Two half-steps cool exactly as much as one full step.
        let full = cool(1.0, REFERENCE_DT);
        let halves = cool(cool(1.0, REFERENCE_DT / 2.0), REFERENCE_DT / 2.0);
        assert!((full - halves).abs() < 1e-6);
        assert!((full - COOLING).abs() < 1e-6);
        assert_eq!(cool(0.5, 0.0), 0.5);
        assert_eq!(cool(0.5, -1.0), 0.5, "a negative dt never heats");
    }

    /// A graph of clusters, as a project's imports make one: dense groups
    /// of files, a few links between them. Its nodes, and its edges.
    fn clustered() -> (Vec<NodeInput>, Vec<(usize, usize)>) {
        let sizes = [30usize, 25, 20, 20, 15, 10];
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut start = 0;
        for (c, &size) in sizes.iter().enumerate() {
            for i in 0..size {
                nodes.push(ni(&format!("c{c}n{i}")));
                // Each file imports three of its cluster's: a dense group.
                for j in 1..=3 {
                    let to = start + (i * 7 + j * 5) % size;
                    if to != start + i {
                        edges.push((start + i, to));
                    }
                }
            }
            // And its cluster's first file one of the last cluster's.
            if c > 0 {
                edges.push((start, start - 1));
            }
            start += size;
        }
        (nodes, edges)
    }

    /// The live simulation run as the canvas runs it, from the canvas's
    /// seed, until it comes to rest: where the nodes end up.
    fn settled(nodes: Vec<NodeInput>, edges: Vec<(usize, usize)>) -> Vec<V3> {
        let laid = layout(nodes, edges);
        let n = laid.nodes.len();
        let mut pos: Vec<V3> = laid
            .nodes
            .iter()
            .enumerate()
            .map(|(i, nd)| {
                let z = (((i * 61) % 100) as f32 / 100.0 - 0.5) * WORLD * 0.8;
                [(nd.x - 0.5) * WORLD, (nd.y - 0.5) * WORLD, z]
            })
            .collect();
        let mut vel = vec![[0.0f32; 3]; n];
        let k = ideal_k(n);
        let mut alpha = 1.0f32;
        let dt = 4.0 / 60.0;
        while alpha > 0.02 {
            fr_step3(&mut pos, &mut vel, &laid.edges, None, k, alpha, dt);
            alpha = cool(alpha, dt);
        }
        pos
    }

    /// How crowded a settled graph is on screen: the share of nodes whose
    /// nearest other node is closer than `near` of the graph's own radius
    /// (its 90th percentile distance from its centre) — on a screen that
    /// fits the graph, as good as on top of it.
    fn crowded(pos: &[V3], near: f32) -> f32 {
        let n = pos.len() as f32;
        let c: V3 = std::array::from_fn(|a| pos.iter().map(|p| p[a]).sum::<f32>() / n);
        let dist = |p: &V3, q: &V3| {
            ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt()
        };
        let mut radii: Vec<f32> = pos.iter().map(|p| dist(p, &c)).collect();
        radii.sort_by(f32::total_cmp);
        let radius = radii[((radii.len() - 1) as f32 * 0.9) as usize];
        let crowded = pos
            .iter()
            .enumerate()
            .filter(|&(i, p)| {
                pos.iter()
                    .enumerate()
                    .filter(|&(j, _)| j != i)
                    .map(|(_, q)| dist(p, q))
                    .fold(f32::MAX, f32::min)
                    < near * radius
            })
            .count();
        crowded as f32 / n
    }

    /// A graph of clusters settles with room between its nodes, on a screen
    /// that fits it: no node has another within 6% of the graph's radius,
    /// and half have none within a tenth of it, about. With the centre's
    /// pull as weak as it was, loosely linked files floated far out, and a
    /// quarter of the nodes were that near one another — drawn, overlapping.
    #[test]
    fn a_settled_graph_leaves_room_between_its_nodes() {
        let (nodes, edges) = clustered();
        let pos = settled(nodes, edges);
        assert_eq!(crowded(&pos, 0.06), 0.0);
        assert!(nearest(&pos) > 0.095, "{}", nearest(&pos));
    }

    /// The median distance from a node to its nearest, as a share of the
    /// graph's radius (its 90th percentile distance from its centre).
    fn nearest(pos: &[V3]) -> f32 {
        let n = pos.len() as f32;
        let c: V3 = std::array::from_fn(|a| pos.iter().map(|p| p[a]).sum::<f32>() / n);
        let dist = |p: &V3, q: &V3| {
            ((p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2)).sqrt()
        };
        let mut radii: Vec<f32> = pos.iter().map(|p| dist(p, &c)).collect();
        radii.sort_by(f32::total_cmp);
        let radius = radii[((radii.len() - 1) as f32 * 0.9) as usize];
        let mut nn: Vec<f32> = pos
            .iter()
            .enumerate()
            .map(|(i, p)| {
                pos.iter()
                    .enumerate()
                    .filter(|&(j, _)| j != i)
                    .map(|(_, q)| dist(p, q))
                    .fold(f32::MAX, f32::min)
            })
            .collect();
        nn.sort_by(f32::total_cmp);
        nn[nn.len() / 2] / radius
    }

    #[test]
    fn live_step_settles_a_perturbed_graph() {
        // A small connected graph, started jittered off a sphere, should lose
        // kinetic energy over time as the live sim cools (alpha decays).
        let n = 8;
        let edges: Vec<(usize, usize)> = (0..n).map(|i| (i, (i + 1) % n)).collect();
        let mut pos: Vec<V3> = (0..n)
            .map(|i| {
                let a = TAU * i as f32 / n as f32;
                // Deterministic jitter so there is energy to dissipate.
                let r = 300.0 + ((i * 71) % 53) as f32;
                let z = ((i * 53) % 41) as f32 - 20.0;
                [r * a.cos(), r * a.sin(), z]
            })
            .collect();
        let mut vel = vec![[0.0f32; 3]; n];
        let k = ideal_k(n);
        let mut alpha = 1.0f32;
        let mut last = f32::MAX;
        for _ in 0..400 {
            last = fr_step3(&mut pos, &mut vel, &edges, None, k, alpha, 0.02);
            alpha *= 0.97;
        }
        assert!(last < 1.0, "graph did not settle: energy {last}");
        // A pinned node never moves.
        let before = pos[0];
        fr_step3(&mut pos, &mut vel, &edges, Some(0), k, 1.0, 0.02);
        assert_eq!(pos[0], before, "pinned node moved");
    }
}
