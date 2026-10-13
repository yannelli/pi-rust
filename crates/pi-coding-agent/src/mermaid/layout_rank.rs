//! Ranking, crossing reduction, cross-axis placement and track packing (upstream
//! `layout.ts`, Sugiyama outline).

use super::graph::{Edge, Graph};

/// Longest-path ranking over the graph's DAG. Back edges found by a DFS colouring
/// pass are excluded, so `A --> B --> C --> A` still ranks 0, 1, 2.
pub(super) fn compute_ranks(graph: &Graph) -> Vec<usize> {
    let n = graph.nodes.len();
    let mut children = vec![Vec::new(); n];
    let mut indeg = vec![0usize; n];
    for e in &graph.edges {
        if e.from != e.to {
            children[e.from].push(e.to);
            indeg[e.to] += 1;
        }
    }

    let mut color = vec![0u8; n];
    let mut dag = vec![Vec::new(); n];
    let mut order = Vec::with_capacity(n);
    // Roots first so ranks grow from natural entry points, then any leftovers.
    let roots: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    for start in roots.into_iter().chain(0..n) {
        if color[start] == 0 {
            dfs_dag(start, &children, &mut color, &mut dag, &mut order);
        }
    }

    let mut rank = vec![0usize; n];
    for &u in order.iter().rev() {
        for &v in &dag[u] {
            rank[v] = rank[v].max(rank[u] + 1);
        }
    }
    rank
}

/// Iterative DFS recording postorder and skipping edges back into the stack.
fn dfs_dag(start: usize, children: &[Vec<usize>], color: &mut [u8], dag: &mut [Vec<usize>], order: &mut Vec<usize>) {
    let mut stack = vec![(start, 0usize)];
    color[start] = 1;
    while let Some(frame) = stack.last_mut() {
        let u = frame.0;
        if frame.1 < children[u].len() {
            let v = children[u][frame.1];
            frame.1 += 1;
            if color[v] == 1 {
                continue;
            }
            dag[u].push(v);
            if color[v] == 0 {
                color[v] = 1;
                stack.push((v, 0));
            }
        } else {
            color[u] = 2;
            order.push(u);
            stack.pop();
        }
    }
}

/// Forward neighbours: `(parents, children)` over edges that go down the ranks.
fn forward_neighbours(edges: &[Edge], ranks: &[usize]) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let n = ranks.len();
    let mut parents = vec![Vec::new(); n];
    let mut children = vec![Vec::new(); n];
    for e in edges {
        if e.from != e.to && ranks[e.to] > ranks[e.from] {
            parents[e.to].push(e.from);
            children[e.from].push(e.to);
        }
    }
    (parents, children)
}

/// Mean of the neighbours' positions, or the node's own position when it has none.
fn barycenter(v: usize, neigh: &[Vec<usize>], pos: impl Fn(usize) -> f64) -> f64 {
    if neigh[v].is_empty() {
        return pos(v);
    }
    neigh[v].iter().fold(0.0, |sum, &u| sum + pos(u)) / neigh[v].len() as f64
}

/// Reorder nodes within each rank to cut edge crossings with barycenter sweeps,
/// keeping whichever ordering crossed least.
pub(super) fn order_ranks(by_rank: &mut [Vec<usize>], edges: &[Edge], ranks: &[usize]) {
    let n = ranks.len();
    if by_rank.len() < 2 || n < 3 {
        return;
    }
    let (parents, children) = forward_neighbours(edges, ranks);
    let mut pos = vec![0usize; n];
    let reindex = |row: &[usize], pos: &mut [usize]| {
        for (i, &v) in row.iter().enumerate() {
            pos[v] = i;
        }
    };
    for row in by_rank.iter() {
        reindex(row, &mut pos);
    }

    let mut best = by_rank.to_vec();
    let mut best_crossings = count_crossings(edges, ranks, &pos);
    if best_crossings == 0 {
        return;
    }

    let last = by_rank.len() - 1;
    for it in 0..8 {
        // Alternate sweeping down (sort by parents) and up (sort by children).
        let (rows, neigh): (Vec<usize>, _) =
            if it % 2 == 0 { ((1..=last).collect(), &parents) } else { ((0..last).rev().collect(), &children) };
        for r in rows {
            let mut keyed: Vec<(f64, usize)> =
                by_rank[r].iter().map(|&v| (barycenter(v, neigh, |u| pos[u] as f64), v)).collect();
            keyed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            by_rank[r] = keyed.into_iter().map(|(_, v)| v).collect();
            reindex(&by_rank[r], &mut pos);
        }
        let crossings = count_crossings(edges, ranks, &pos);
        if crossings < best_crossings {
            best_crossings = crossings;
            best = by_rank.to_vec();
        }
        if best_crossings == 0 {
            break;
        }
    }
    by_rank.clone_from_slice(&best);
}

fn count_crossings(edges: &[Edge], ranks: &[usize], pos: &[usize]) -> usize {
    let spans: Vec<(usize, usize, usize)> = edges
        .iter()
        .filter(|e| e.from != e.to && ranks[e.to] == ranks[e.from] + 1)
        .map(|e| (ranks[e.from], pos[e.from], pos[e.to]))
        .collect();
    let mut crossings = 0;
    for (i, a) in spans.iter().enumerate() {
        for b in &spans[i + 1..] {
            if a.0 == b.0 && ((a.1 < b.1 && a.2 > b.2) || (a.1 > b.1 && a.2 < b.2)) {
                crossings += 1;
            }
        }
    }
    crossings
}

/// JavaScript `Math.round`: halves round toward positive infinity.
fn js_round(x: f64) -> f64 {
    let floor = x.floor();
    if x - floor >= 0.5 { floor + 1.0 } else { floor }
}

/// Assign a cross-axis centre to every node so it drifts toward the mean of its
/// neighbours, while ranks keep their order and boxes keep `sep` between them.
pub(super) fn assign_positions(
    by_rank: &[Vec<usize>],
    size: &[usize],
    sep: usize,
    edges: &[Edge],
    ranks: &[usize],
) -> Vec<usize> {
    let n = size.len();
    let (parents, children) = forward_neighbours(edges, ranks);
    let sep = sep as f64;
    let mut pos = vec![0.0f64; n];
    for row in by_rank {
        let mut x = 0.0;
        for &v in row {
            let half = size[v] as f64 / 2.0;
            x += half;
            pos[v] = x;
            x += half + sep;
        }
    }

    for it in 0..10 {
        if it % 2 == 0 {
            for row in by_rank {
                relax_rank(row, &parents, &mut pos, size, sep);
            }
        } else {
            for row in by_rank.iter().rev() {
                relax_rank(row, &children, &mut pos, size, sep);
            }
        }
    }

    let mut min_left = f64::INFINITY;
    for v in 0..n {
        min_left = min_left.min(pos[v] - size[v] as f64 / 2.0);
    }
    if !min_left.is_finite() {
        min_left = 0.0;
    }
    (0..n).map(|v| js_round(pos[v] - min_left).max(0.0) as usize).collect()
}

/// Sweep right then left and take the midpoint, centring each node between the
/// tightest packings that respect order from either side.
fn relax_rank(nodes: &[usize], neigh: &[Vec<usize>], pos: &mut [f64], size: &[usize], sep: f64) {
    let n = nodes.len();
    if n == 0 {
        return;
    }
    let desired: Vec<f64> = nodes.iter().map(|&v| barycenter(v, neigh, |u| pos[u])).collect();
    let half_of = |i: usize| size[nodes[i]] as f64 / 2.0;

    let mut left = vec![0.0f64; n];
    for i in 0..n {
        left[i] = if i == 0 { desired[i] } else { desired[i].max(left[i - 1] + half_of(i - 1) + sep + half_of(i)) };
    }
    let mut right = vec![0.0f64; n];
    for i in (0..n).rev() {
        right[i] =
            if i == n - 1 { desired[i] } else { desired[i].min(right[i + 1] - half_of(i + 1) - sep - half_of(i)) };
    }
    for i in 0..n {
        pos[nodes[i]] = (left[i] + right[i]) / 2.0;
    }
    for i in 1..n {
        let min_p = pos[nodes[i - 1]] + half_of(i - 1) + sep + half_of(i);
        if pos[nodes[i]] < min_p {
            pos[nodes[i]] = min_p;
        }
    }
}

/// A span competing for a track: `[start, end, from, to, edge_index]`.
pub(super) type Span5 = [usize; 5];

/// Pack spans into as few parallel tracks as possible. Two spans share a track when
/// they are two cells apart, or when they share an endpoint, so a fan-out reuses
/// one row and a merge draws one arrowhead. Returns `(edge, track)` pairs and a count.
pub(super) fn assign_tracks(spans: &[Span5]) -> (Vec<(usize, usize)>, usize) {
    let mut sorted = spans.to_vec();
    sorted.sort();
    let mut tracks: Vec<Vec<[usize; 4]>> = Vec::new();
    let mut assigned = Vec::with_capacity(sorted.len());
    for [s, e, f, t, idx] in sorted {
        let fits = |members: &Vec<[usize; 4]>| {
            members.iter().all(|&[s2, e2, f2, t2]| e2 + 2 <= s || e + 2 <= s2 || f2 == f || t2 == t)
        };
        let slot = match tracks.iter().position(fits) {
            Some(slot) => slot,
            None => {
                tracks.push(Vec::new());
                tracks.len() - 1
            }
        };
        tracks[slot].push([s, e, f, t]);
        assigned.push((idx, slot));
    }
    (assigned, tracks.len())
}
