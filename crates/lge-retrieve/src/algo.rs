//! Graph and ranking algorithms on plain in-memory data. No database access:
//! everything here is a pure, deterministic function, so results are cacheable
//! and testable.
//!
//! The LadybugDB `algo` extension does not load on macOS arm64 (see
//! `docs/PLAN.md`, risks), so PageRank and community detection live here.

use std::collections::{BTreeMap, BTreeSet, HashMap};

// ---------------------------------------------------------------------------
// Rank fusion and diversity
// ---------------------------------------------------------------------------

/// The reciprocal-rank-fusion constant used by hindsight and omnigraph.
pub const RRF_K: f64 = 60.0;

/// Reciprocal rank fusion of several ranked id lists, each with a weight:
/// `score(id) = sum over lists of weight / (k + rank)`, rank starting at 1.
/// Highest score first; ties are broken by id so the order is stable.
pub fn rrf(lists: &[(f64, Vec<String>)], k: f64) -> Vec<(String, f64)> {
    let mut scores: HashMap<&str, f64> = HashMap::new();
    for (weight, list) in lists {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut rank = 0.0;
        for id in list {
            // A repeated id keeps its best (first) rank in that list.
            if seen.insert(id.as_str()) {
                rank += 1.0;
                *scores.entry(id.as_str()).or_insert(0.0) += weight / (k + rank);
            }
        }
    }
    let mut out: Vec<(String, f64)> = scores.into_iter().map(|(id, s)| (id.to_string(), s)).collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// Jaccard similarity of two word sets; 0 when both are empty.
pub fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let inter = a.intersection(b).count();
    let union = a.len() + b.len() - inter;
    if union == 0 {
        0.0
    } else {
        inter as f64 / union as f64
    }
}

/// Greedy maximal marginal relevance over word sets: repeatedly take the item
/// with the highest `lambda * relevance - (1 - lambda) * max similarity to the
/// items already taken`. Drops near-duplicates before they cost tokens.
/// `items` are (index into the caller's data, relevance in 0..=1, words).
pub fn mmr(items: &[(usize, f64, BTreeSet<String>)], lambda: f64, limit: usize) -> Vec<usize> {
    let mut left: Vec<usize> = (0..items.len()).collect();
    let mut taken: Vec<usize> = Vec::new();
    while taken.len() < limit && !left.is_empty() {
        let mut best: Option<(usize, f64)> = None;
        for (pos, &i) in left.iter().enumerate() {
            let redundancy = taken
                .iter()
                .map(|&t| jaccard(&items[i].2, &items[t].2))
                .fold(0.0, f64::max);
            let score = lambda * items[i].1 - (1.0 - lambda) * redundancy;
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((pos, score));
            }
        }
        let Some((pos, _)) = best else { break };
        taken.push(left.remove(pos));
    }
    taken.into_iter().map(|i| items[i].0).collect()
}

// ---------------------------------------------------------------------------
// PageRank
// ---------------------------------------------------------------------------

/// Weighted PageRank by power iteration over `n` nodes and directed
/// `(from, to, weight)` edges: rank flows from `from` to `to`.
///
/// `restart` is the distribution a random walk jumps back to (and where the
/// rank of nodes without out-edges goes). `None` is uniform: the global rank.
/// A distribution concentrated on a few nodes gives personalized PageRank:
/// proximity to those nodes. The result sums to 1 (or is all zero for `n = 0`).
pub fn pagerank(
    n: usize,
    edges: &[(usize, usize, f64)],
    damping: f64,
    iterations: usize,
    restart: Option<&[f64]>,
) -> Vec<f64> {
    if n == 0 {
        return Vec::new();
    }
    let uniform = vec![1.0 / n as f64; n];
    let restart: Vec<f64> = match restart {
        Some(r) if r.len() == n && r.iter().sum::<f64>() > 0.0 => {
            let total: f64 = r.iter().sum();
            r.iter().map(|x| x / total).collect()
        }
        _ => uniform,
    };
    let mut out_weight = vec![0.0; n];
    for &(from, to, w) in edges {
        if from < n && to < n && from != to && w > 0.0 {
            out_weight[from] += w;
        }
    }
    let mut rank = restart.clone();
    for _ in 0..iterations {
        let mut next = vec![0.0; n];
        let dangling: f64 = (0..n).filter(|&i| out_weight[i] == 0.0).map(|i| rank[i]).sum();
        for &(from, to, w) in edges {
            if from < n && to < n && from != to && w > 0.0 {
                next[to] += damping * rank[from] * w / out_weight[from];
            }
        }
        let mut delta = 0.0;
        for i in 0..n {
            next[i] += (1.0 - damping + damping * dangling) * restart[i];
            delta += (next[i] - rank[i]).abs();
        }
        rank = next;
        if delta < 1e-10 {
            break;
        }
    }
    rank
}

// ---------------------------------------------------------------------------
// Communities
// ---------------------------------------------------------------------------

/// Louvain community detection on an undirected weighted graph given as
/// `(a, b, weight)` pairs (direction is ignored, duplicates add up).
///
/// Returns the community of each node. Communities are numbered by size,
/// largest first (ties: the one holding the lowest node index first), so the
/// numbering is stable for a given graph. A node without edges is alone.
pub fn louvain(n: usize, edges: &[(usize, usize, f64)]) -> Vec<usize> {
    // adj[u][v] = weight; a self entry holds twice the internal weight, so the
    // degree of a node is always the sum of its row.
    let mut adj: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); n];
    for &(a, b, w) in edges {
        if a < n && b < n && a != b && w > 0.0 {
            *adj[a].entry(b).or_insert(0.0) += w;
            *adj[b].entry(a).or_insert(0.0) += w;
        }
    }
    // level_node[o] = the node of the current level that original node `o` is in.
    let mut level_node: Vec<usize> = (0..n).collect();
    loop {
        let (assign, communities, moved) = local_moving(&adj);
        if !moved {
            break;
        }
        for node in level_node.iter_mut() {
            *node = assign[*node];
        }
        let mut next: Vec<BTreeMap<usize, f64>> = vec![BTreeMap::new(); communities];
        for (u, row) in adj.iter().enumerate() {
            for (&v, &w) in row {
                *next[assign[u]].entry(assign[v]).or_insert(0.0) += w;
            }
        }
        if communities == adj.len() {
            break;
        }
        adj = next;
    }
    renumber_by_size(&level_node)
}

/// One Louvain level: move nodes between communities while modularity rises.
/// Returns (compact community per node, number of communities, whether any node moved).
fn local_moving(adj: &[BTreeMap<usize, f64>]) -> (Vec<usize>, usize, bool) {
    let n = adj.len();
    let degree: Vec<f64> = adj.iter().map(|row| row.values().sum()).collect();
    let total: f64 = degree.iter().sum();
    let mut community: Vec<usize> = (0..n).collect();
    let mut community_degree = degree.clone();
    let mut moved_any = false;
    if total > 0.0 {
        for _ in 0..32 {
            let mut moved = false;
            for i in 0..n {
                let own = community[i];
                community_degree[own] -= degree[i];
                let mut to: BTreeMap<usize, f64> = BTreeMap::new();
                for (&v, &w) in &adj[i] {
                    if v != i {
                        *to.entry(community[v]).or_insert(0.0) += w;
                    }
                }
                let gain = |c: usize, weight: f64| weight - community_degree[c] * degree[i] / total;
                let mut best = (own, gain(own, to.get(&own).copied().unwrap_or(0.0)));
                for (&c, &weight) in &to {
                    let g = gain(c, weight);
                    if g > best.1 + 1e-12 {
                        best = (c, g);
                    }
                }
                community_degree[best.0] += degree[i];
                if best.0 != own {
                    community[i] = best.0;
                    moved = true;
                    moved_any = true;
                }
            }
            if !moved {
                break;
            }
        }
    }
    let mut compact: BTreeMap<usize, usize> = BTreeMap::new();
    for &c in &community {
        let next = compact.len();
        compact.entry(c).or_insert(next);
    }
    let assign: Vec<usize> = community.iter().map(|c| compact[c]).collect();
    (assign, compact.len(), moved_any)
}

/// Renumber labels so that 0 is the largest group.
fn renumber_by_size(label: &[usize]) -> Vec<usize> {
    let mut groups: BTreeMap<usize, (usize, usize)> = BTreeMap::new(); // label -> (size, lowest member)
    for (i, &l) in label.iter().enumerate() {
        let g = groups.entry(l).or_insert((0, i));
        g.0 += 1;
    }
    let mut order: Vec<(usize, (usize, usize))> = groups.into_iter().collect();
    order.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.1 .1.cmp(&b.1 .1)));
    let new_id: HashMap<usize, usize> = order.iter().enumerate().map(|(i, (l, _))| (*l, i)).collect();
    label.iter().map(|l| new_id[l]).collect()
}

/// Share of the possible pairs inside a group that are connected: 1 for a
/// clique, near 0 for a loose group. `members` and the edge ends are node indices.
pub fn cohesion(members: &BTreeSet<usize>, edges: &[(usize, usize, f64)]) -> f64 {
    let n = members.len();
    if n < 2 {
        return 1.0;
    }
    let mut pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    for &(a, b, _) in edges {
        if a != b && members.contains(&a) && members.contains(&b) {
            pairs.insert((a.min(b), a.max(b)));
        }
    }
    pairs.len() as f64 / (n * (n - 1) / 2) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rrf_rewards_agreement_and_is_stable() {
        let fused = rrf(&[(1.0, ids(&["a", "b", "c"])), (1.0, ids(&["b", "a", "d"]))], RRF_K);
        let order: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            order,
            ["a", "b", "c", "d"],
            "a and b tie and sort by id; c and d tie and sort by id"
        );
        assert!((fused[0].1 - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-12);
        // A weight moves a list's votes; a repeated id counts once.
        let fused = rrf(&[(1.0, ids(&["a", "a", "b"])), (3.0, ids(&["b"]))], RRF_K);
        assert_eq!(fused[0].0, "b");
        assert!((fused[1].1 - 1.0 / 61.0).abs() < 1e-12);
        assert!(rrf(&[], RRF_K).is_empty());
    }

    fn words(s: &str) -> BTreeSet<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn mmr_drops_near_duplicates() {
        let items = vec![
            (0, 1.0, words("create order handler validates payload")),
            (1, 0.95, words("create order handler validates payload too")),
            (2, 0.6, words("send email smtp mailer")),
        ];
        assert_eq!(mmr(&items, 0.5, 2), [0, 2]);
        assert_eq!(mmr(&items, 1.0, 2), [0, 1], "lambda 1 is plain relevance order");
        assert_eq!(mmr(&items, 0.5, 10).len(), 3);
        assert!(mmr(&[], 0.5, 3).is_empty());
        assert_eq!(jaccard(&words("a b"), &words("a b")), 1.0);
        assert_eq!(jaccard(&words("a b"), &words("c d")), 0.0);
        assert_eq!(jaccard(&words(""), &words("")), 0.0);
    }

    #[test]
    fn pagerank_follows_edges_and_sums_to_one() {
        // 0 -> 2, 1 -> 2, 2 -> 3: rank piles up downstream.
        let edges = [(0, 2, 1.0), (1, 2, 1.0), (2, 3, 1.0)];
        let r = pagerank(4, &edges, 0.85, 100, None);
        assert!((r.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(r[3] > r[2] && r[2] > r[0] && (r[0] - r[1]).abs() < 1e-12, "{r:?}");
        // Personalized: restart at node 0 only; node 1 is unreachable from it.
        let p = pagerank(4, &edges, 0.85, 100, Some(&[1.0, 0.0, 0.0, 0.0]));
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(p[1] < 1e-12 && p[2] > 0.0 && p[0] > p[1], "{p:?}");
        assert!(pagerank(0, &[], 0.85, 10, None).is_empty());
        // Bad input is ignored, not a panic.
        let r = pagerank(
            2,
            &[(0, 9, 1.0), (0, 0, 1.0), (0, 1, -1.0)],
            0.85,
            10,
            Some(&[0.0, 0.0]),
        );
        assert!((r.iter().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn louvain_finds_two_cliques_joined_by_one_edge() {
        let mut edges = Vec::new();
        for group in [[0, 1, 2, 3], [4, 5, 6, 7]] {
            for a in group {
                for b in group {
                    if a < b {
                        edges.push((a, b, 1.0));
                    }
                }
            }
        }
        edges.push((3, 4, 1.0));
        let c = louvain(9, &edges);
        assert_eq!(&c[0..4], [0, 0, 0, 0]);
        assert_eq!(&c[4..8], [1, 1, 1, 1]);
        assert_eq!(c[8], 2, "the isolated node is its own community");
        // Same graph, edges reversed and shuffled: same answer.
        let mut shuffled: Vec<(usize, usize, f64)> = edges.iter().rev().map(|&(a, b, w)| (b, a, w)).collect();
        shuffled.swap(0, 5);
        assert_eq!(louvain(9, &shuffled), c);
        assert!(louvain(0, &[]).is_empty());
        assert_eq!(louvain(3, &[]), [0, 1, 2]);
    }

    #[test]
    fn cohesion_is_the_share_of_connected_pairs() {
        let members: BTreeSet<usize> = [0, 1, 2].into_iter().collect();
        assert_eq!(cohesion(&members, &[(0, 1, 1.0), (1, 2, 1.0), (2, 0, 1.0)]), 1.0);
        assert!((cohesion(&members, &[(0, 1, 1.0), (1, 0, 1.0), (2, 9, 1.0)]) - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(cohesion(&[0].into_iter().collect(), &[]), 1.0);
    }
}
