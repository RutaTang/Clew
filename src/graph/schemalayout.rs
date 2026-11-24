//! Where a schema's tables go on its diagram.
//!
//! Each group of tables joined by foreign keys is laid out on its own, in
//! columns: a table that references another stands to its left, so the
//! lines read left to right, from a key to what it points at, and a table
//! many point at stands to the right of them all. A table that points at
//! others but nothing points at stands one column before the first of them,
//! not at the far left with the longest chains' starts. Within a column the
//! tables are ordered to cross as few lines as can be found, then moved up
//! or down toward the tables they are joined to. The groups, largest first,
//! and the tables joined to nothing, as a grid, are packed in rows.

/// A card's size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Size {
    pub w: f32,
    pub h: f32,
}

/// The room between two columns, where their lines run.
pub(crate) const COLUMN_GAP: f32 = 110.0;
/// The room between two cards of a column.
pub(crate) const CARD_GAP: f32 = 36.0;
/// The room between two groups.
const GROUP_GAP: f32 = 90.0;

/// A block of cards laid out together: each card's place in it, and its
/// width and height.
type Block = (Vec<(usize, f32, f32)>, f32, f32);

/// The room between two parts of one column wrapped to fit the view.
const WRAP_GAP: f32 = 48.0;

/// Each card's top-left corner, for `cards` joined by `links` (each
/// `(from, to)`: a card that references another, and that one), the whole
/// about `aspect` times as wide as tall where the tables allow.
pub(crate) fn place(cards: &[Size], links: &[(usize, usize)], aspect: f32) -> Vec<(f32, f32)> {
    let aspect = aspect.clamp(0.5, 4.0);
    let n = cards.len();
    let mut out = vec![(0.0, 0.0); n];
    if n == 0 {
        return out;
    }
    let mut links: Vec<(usize, usize)> = links
        .iter()
        .copied()
        .filter(|&(a, b)| a != b && a < n && b < n)
        .collect();
    links.sort_unstable();
    links.dedup();
    // Groups: tables joined to one another, either way.
    let mut parent: Vec<usize> = (0..n).collect();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for &(a, b) in &links {
        let (ra, rb) = (root(&mut parent, a), root(&mut parent, b));
        if ra != rb {
            parent[ra.max(rb)] = ra.min(rb);
        }
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut group_of_root = vec![usize::MAX; n];
    let mut group_of = vec![0usize; n];
    for i in 0..n {
        let r = root(&mut parent, i);
        if group_of_root[r] == usize::MAX {
            group_of_root[r] = groups.len();
            groups.push(Vec::new());
        }
        group_of[i] = group_of_root[r];
        groups[group_of[i]].push(i);
    }
    // Each block: its cards' places within it, and its size.
    let mut blocks: Vec<Block> = Vec::new();
    let mut loose: Vec<usize> = Vec::new();
    for (g, members) in groups.iter().enumerate() {
        if members.len() == 1 {
            loose.push(members[0]);
            continue;
        }
        let inner: Vec<(usize, usize)> = links
            .iter()
            .copied()
            .filter(|&(a, _)| group_of[a] == g)
            .collect();
        blocks.push(layered(cards, members, &inner, aspect));
    }
    // Largest first; the tables joined to nothing after them, as a grid.
    blocks.sort_by(|a, b| (b.1 * b.2).total_cmp(&(a.1 * a.2)));
    if !loose.is_empty() {
        blocks.push(grid(cards, &loose));
    }
    // Packed in rows about as wide as the whole is tall, the widest block
    // setting the least.
    let area: f32 = blocks
        .iter()
        .map(|b| (b.1 + GROUP_GAP) * (b.2 + GROUP_GAP))
        .sum();
    let widest = blocks.iter().map(|b| b.1).fold(0.0, f32::max);
    let row_width = widest.max((area * aspect).sqrt());
    let (mut x, mut y, mut row_h) = (0.0f32, 0.0f32, 0.0f32);
    for (placed, w, h) in blocks {
        if x > 0.0 && x + w > row_width {
            x = 0.0;
            y += row_h + GROUP_GAP;
            row_h = 0.0;
        }
        for (i, cx, cy) in placed {
            out[i] = (x + cx, y + cy);
        }
        x += w + GROUP_GAP;
        row_h = row_h.max(h);
    }
    out
}

/// The tables joined to nothing, in rows of about equal width and height.
fn grid(cards: &[Size], members: &[usize]) -> Block {
    let area: f32 = members
        .iter()
        .map(|&i| (cards[i].w + CARD_GAP) * (cards[i].h + CARD_GAP))
        .sum();
    let widest = members.iter().map(|&i| cards[i].w).fold(0.0, f32::max);
    let row_width = widest.max(area.sqrt() * 1.3);
    // Columns of cards, each as wide as its widest, filled top to bottom.
    let mut out = Vec::new();
    let (mut x, mut y, mut col_w) = (0.0f32, 0.0f32, 0.0f32);
    let col_height =
        (area / row_width).max(members.iter().map(|&i| cards[i].h).fold(0.0, f32::max));
    let (mut width, mut height) = (0.0f32, 0.0f32);
    for &i in members {
        let c = cards[i];
        if y > 0.0 && y + c.h > col_height {
            x += col_w + CARD_GAP;
            y = 0.0;
            col_w = 0.0;
        }
        out.push((i, x, y));
        width = width.max(x + c.w);
        height = height.max(y + c.h);
        y += c.h + CARD_GAP;
        col_w = col_w.max(c.w);
    }
    (out, width, height)
}

/// One group laid out in columns: its cards' places, and its size.
fn layered(
    cards: &[Size],
    members: &[usize],
    links: &[(usize, usize)],
    aspect: f32,
) -> Block {
    let local: std::collections::HashMap<usize, usize> =
        members.iter().enumerate().map(|(k, &i)| (i, k)).collect();
    let m = members.len();
    let mut out_edges: Vec<Vec<usize>> = vec![Vec::new(); m];
    let mut in_edges: Vec<Vec<usize>> = vec![Vec::new(); m];
    for &(a, b) in links {
        let (Some(&a), Some(&b)) = (local.get(&a), local.get(&b)) else {
            continue;
        };
        out_edges[a].push(b);
        in_edges[b].push(a);
    }
    // A cycle (tables that reference one another) is cut where a walk
    // from the first table meets it again.
    let mut kept: Vec<Vec<usize>> = vec![Vec::new(); m];
    let mut state = vec![0u8; m];
    for start in 0..m {
        if state[start] != 0 {
            continue;
        }
        let mut stack = vec![(start, 0usize)];
        state[start] = 1;
        while let Some((v, next)) = stack.pop() {
            if let Some(&w) = out_edges[v].get(next) {
                stack.push((v, next + 1));
                match state[w] {
                    0 => {
                        kept[v].push(w);
                        state[w] = 1;
                        stack.push((w, 0));
                    }
                    2 => kept[v].push(w),
                    _ => {}
                }
            } else {
                state[v] = 2;
            }
        }
    }
    let mut parents_of: Vec<Vec<usize>> = vec![Vec::new(); m];
    for (v, ws) in kept.iter().enumerate() {
        for &w in ws {
            parents_of[w].push(v);
        }
    }
    // Columns: each table right of every table that references it.
    let mut rank = vec![0usize; m];
    let order = topological(&kept);
    for &v in &order {
        for &w in &kept[v] {
            rank[w] = rank[w].max(rank[v] + 1);
        }
    }
    // A table nothing references stands just before the first of those it
    // references.
    for v in 0..m {
        if parents_of[v].is_empty()
            && let Some(first) = kept[v].iter().map(|&w| rank[w]).min()
        {
            rank[v] = first.saturating_sub(1);
        }
    }
    let columns = rank.iter().copied().max().unwrap_or(0) + 1;
    let mut column: Vec<Vec<usize>> = vec![Vec::new(); columns];
    for v in 0..m {
        column[rank[v]].push(v);
    }
    // Order in each column by where its neighbours are, a few sweeps each way.
    let neighbours: Vec<Vec<usize>> = (0..m)
        .map(|v| out_edges[v].iter().chain(&in_edges[v]).copied().collect())
        .collect();
    let mut pos = vec![0.0f32; m];
    let set_pos = |column: &[Vec<usize>], pos: &mut [f32]| {
        for col in column {
            for (k, &v) in col.iter().enumerate() {
                pos[v] = k as f32;
            }
        }
    };
    set_pos(&column, &mut pos);
    for sweep in 0..6 {
        let range: Vec<usize> = match sweep % 2 {
            0 => (0..columns).collect(),
            _ => (0..columns).rev().collect(),
        };
        for r in range {
            let mut keyed: Vec<(f32, usize)> = column[r]
                .iter()
                .map(|&v| {
                    let near: Vec<f32> = neighbours[v]
                        .iter()
                        .filter(|&&w| rank[w] != r)
                        .map(|&w| pos[w])
                        .collect();
                    let at = match near.is_empty() {
                        true => pos[v],
                        false => near.iter().sum::<f32>() / near.len() as f32,
                    };
                    (at, v)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            column[r] = keyed.into_iter().map(|(_, v)| v).collect();
            set_pos(&column, &mut pos);
        }
    }
    // A column taller than the whole should be, for its width, is wrapped
    // into parts side by side.
    let size = |v: usize| cards[members[v]];
    let area: f32 = (0..m)
        .map(|v| (size(v).w + COLUMN_GAP) * (size(v).h + CARD_GAP))
        .sum();
    let tallest = (0..m).map(|v| size(v).h).fold(0.0, f32::max);
    let most = (area / aspect).sqrt().max(tallest);
    // Each part: its cards, and whether it goes on its column's line.
    let mut parts: Vec<(Vec<usize>, bool)> = Vec::new();
    for col in &column {
        let mut part: Vec<usize> = Vec::new();
        let mut height = 0.0f32;
        let mut first = true;
        for &v in col {
            if !part.is_empty() && height + size(v).h > most {
                parts.push((std::mem::take(&mut part), !first));
                first = false;
                height = 0.0;
            }
            height += size(v).h + CARD_GAP;
            part.push(v);
        }
        if !part.is_empty() {
            parts.push((part, !first));
        }
    }
    let mut slot = vec![0usize; m];
    for (k, (part, _)) in parts.iter().enumerate() {
        for &v in part {
            slot[v] = k;
        }
    }
    let column: Vec<Vec<usize>> = parts.iter().map(|(p, _)| p.clone()).collect();
    // x: each part as wide as its widest card; a column's parts closer to
    // one another than to the next column.
    let mut x = vec![0.0f32; column.len()];
    for k in 1..column.len() {
        let widest = column[k - 1].iter().map(|&v| size(v).w).fold(0.0, f32::max);
        let gap = if parts[k].1 { WRAP_GAP } else { COLUMN_GAP };
        x[k] = x[k - 1] + widest + gap;
    }
    // y: stacked, then each card moved toward the middle of those it is
    // joined to, keeping its column's order and gaps.
    let mut top = vec![0.0f32; m];
    for col in &column {
        let mut y = 0.0;
        for &v in col {
            top[v] = y;
            y += size(v).h + CARD_GAP;
        }
    }
    let middle = |v: usize, top: &[f32]| top[v] + size(v).h / 2.0;
    for _ in 0..12 {
        for col in &column {
            let want: Vec<f32> = col
                .iter()
                .map(|&v| {
                    let near: Vec<f32> = neighbours[v].iter().map(|&w| middle(w, &top)).collect();
                    match near.is_empty() {
                        true => top[v],
                        false => near.iter().sum::<f32>() / near.len() as f32 - size(v).h / 2.0,
                    }
                })
                .collect();
            // Down from the top, then up from the bottom, each keeping the
            // gaps; the two averaged.
            let mut down = vec![0.0f32; col.len()];
            let mut floor = f32::MIN;
            for (k, &v) in col.iter().enumerate() {
                down[k] = want[k].max(floor);
                floor = down[k] + size(v).h + CARD_GAP;
            }
            let mut up = vec![0.0f32; col.len()];
            let mut ceiling = f32::MAX;
            for (k, &v) in col.iter().enumerate().rev() {
                up[k] = want[k].min(ceiling - size(v).h);
                ceiling = up[k] - CARD_GAP;
            }
            let mut floor = f32::MIN;
            for (k, &v) in col.iter().enumerate() {
                let y = ((down[k] + up[k]) / 2.0).max(floor);
                top[v] = y;
                floor = y + size(v).h + CARD_GAP;
            }
        }
    }
    let least = top.iter().copied().fold(f32::MAX, f32::min);
    let mut placed = Vec::with_capacity(m);
    let (mut width, mut height) = (0.0f32, 0.0f32);
    for v in 0..m {
        let (cx, cy) = (x[slot[v]], top[v] - least);
        placed.push((members[v], cx, cy));
        width = width.max(cx + size(v).w);
        height = height.max(cy + size(v).h);
    }
    (placed, width, height)
}

/// The nodes of the acyclic `edges` in an order where each comes before
/// the ones it points at.
fn topological(edges: &[Vec<usize>]) -> Vec<usize> {
    let n = edges.len();
    let mut indegree = vec![0usize; n];
    for ws in edges {
        for &w in ws {
            indegree[w] += 1;
        }
    }
    let mut ready: Vec<usize> = (0..n).filter(|&v| indegree[v] == 0).rev().collect();
    let mut out = Vec::with_capacity(n);
    while let Some(v) = ready.pop() {
        out.push(v);
        for &w in &edges[v] {
            indegree[w] -= 1;
            if indegree[w] == 0 {
                ready.push(w);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(w: f32, h: f32) -> Size {
        Size { w, h }
    }

    fn overlaps(cards: &[Size], at: &[(f32, f32)]) -> Option<(usize, usize)> {
        for i in 0..cards.len() {
            for j in i + 1..cards.len() {
                let (a, b) = (at[i], at[j]);
                if a.0 < b.0 + cards[j].w
                    && b.0 < a.0 + cards[i].w
                    && a.1 < b.1 + cards[j].h
                    && b.1 < a.1 + cards[i].h
                {
                    return Some((i, j));
                }
            }
        }
        None
    }

    /// Supabase's storage tables: a key's table left of the one it points
    /// at, the chain read left to right, no two cards over each other.
    #[test]
    fn a_table_stands_left_of_what_it_references() {
        // 0 migrations, 1 parts, 2 uploads, 3 objects, 4 buckets
        let cards = vec![
            card(220.0, 110.0),
            card(240.0, 230.0),
            card(230.0, 210.0),
            card(250.0, 300.0),
            card(240.0, 250.0),
        ];
        let links = [(1, 2), (1, 4), (2, 4), (3, 4)];
        let at = place(&cards, &links, 1.0);
        assert!(at[1].0 < at[2].0 && at[2].0 < at[4].0, "{at:?}");
        assert!(at[3].0 < at[4].0, "{at:?}");
        assert_eq!(
            at[3].0, at[2].0,
            "objects one column before buckets, not at the far left: {at:?}"
        );
        assert_eq!(overlaps(&cards, &at), None, "{at:?}");
    }

    /// A column too tall for a wide view is wrapped into parts.
    #[test]
    fn a_tall_column_is_wrapped_for_a_wide_view() {
        // Twelve tables all referencing one.
        let mut cards = vec![card(220.0, 260.0); 12];
        cards.push(card(220.0, 120.0));
        let links: Vec<(usize, usize)> = (0..12).map(|i| (i, 12)).collect();
        let at = place(&cards, &links, 1.8);
        let width = at
            .iter()
            .zip(&cards)
            .map(|(p, c)| p.0 + c.w)
            .fold(0.0, f32::max);
        let height = at
            .iter()
            .zip(&cards)
            .map(|(p, c)| p.1 + c.h)
            .fold(0.0, f32::max);
        assert!(width / height > 0.8, "{width} × {height}");
        assert!(
            (0..12).all(|i| at[i].0 < at[12].0),
            "every referencing table left of the one: {at:?}"
        );
        assert_eq!(overlaps(&cards, &at), None);
    }

    #[test]
    fn cycles_and_self_references_still_lay_out() {
        let cards = vec![card(200.0, 100.0); 4];
        let links = [(0, 1), (1, 2), (2, 0), (3, 3), (3, 0)];
        let at = place(&cards, &links, 1.6);
        assert_eq!(overlaps(&cards, &at), None, "{at:?}");
        assert!(at.iter().all(|p| p.0.is_finite() && p.1.is_finite()));
    }

    #[test]
    fn many_tables_joined_to_nothing_make_a_grid() {
        let cards: Vec<Size> = (0..30)
            .map(|i| card(180.0 + (i % 3) as f32 * 20.0, 80.0 + (i % 5) as f32 * 30.0))
            .collect();
        let at = place(&cards, &[], 1.6);
        assert_eq!(overlaps(&cards, &at), None);
        let width = at
            .iter()
            .zip(&cards)
            .map(|(p, c)| p.0 + c.w)
            .fold(0.0, f32::max);
        let height = at
            .iter()
            .zip(&cards)
            .map(|(p, c)| p.1 + c.h)
            .fold(0.0, f32::max);
        assert!(
            width / height < 3.0 && height / width < 3.0,
            "{width} × {height}"
        );
    }

    #[test]
    fn a_large_schema_lays_out_without_overlaps_and_quickly() {
        // 300 tables: a tree of references and some loose ones.
        let cards: Vec<Size> = (0..300)
            .map(|i| card(200.0 + (i % 7) as f32 * 15.0, 70.0 + (i % 11) as f32 * 24.0))
            .collect();
        let links: Vec<(usize, usize)> = (1..260).map(|i| (i, (i * 7) % i.max(1))).collect();
        let started = std::time::Instant::now();
        let at = place(&cards, &links, 1.6);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(overlaps(&cards, &at), None);
    }
}
