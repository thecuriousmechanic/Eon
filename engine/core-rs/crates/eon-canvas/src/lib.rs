//! Layered (Sugiyama-style) layout for directed acyclic graphs: rank assignment, dummy nodes for edges that span several
//! ranks, and barycenter crossing reduction (docs/implementation/05, section 5.2).

pub mod cfg;
pub mod diff;
pub mod render;
pub mod view;
pub mod writeback;

use std::cmp::Ordering;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    NodeOutOfRange { node: usize },
    Cycle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Rank (layer) of each real node.
    pub rank: Vec<usize>,
    /// Position of each real node within its rank, counted from zero.
    pub slot: Vec<usize>,
    /// Waypoints of each input edge as (rank, slot), including dummy nodes for edges that span several ranks.
    pub edge_paths: Vec<Vec<(usize, usize)>>,
    /// Edge crossings left after reduction.
    pub crossings: usize,
}

const SWEEPS: usize = 8;

pub fn layered(nodes: usize, edges: &[(usize, usize)]) -> Result<Layout, LayoutError> {
    for &(u, v) in edges {
        for node in [u, v] {
            if node >= nodes {
                return Err(LayoutError::NodeOutOfRange { node });
            }
        }
    }
    let mut node_rank = assign_ranks(nodes, edges)?;

    let mut chains: Vec<Vec<usize>> = Vec::with_capacity(edges.len());
    let mut segments: Vec<(usize, usize)> = Vec::new();
    for &(u, v) in edges {
        let mut chain = vec![u];
        for r in node_rank[u] + 1..node_rank[v] {
            node_rank.push(r);
            chain.push(node_rank.len() - 1);
        }
        chain.push(v);
        segments.extend(chain.windows(2).map(|w| (w[0], w[1])));
        chains.push(chain);
    }

    let total = node_rank.len();
    let layer_count = node_rank.iter().copied().max().map_or(0, |r| r + 1);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); layer_count];
    for (node, &r) in node_rank.iter().enumerate() {
        layers[r].push(node);
    }

    let mut up = vec![Vec::new(); total];
    let mut down = vec![Vec::new(); total];
    for &(a, b) in &segments {
        down[a].push(b);
        up[b].push(a);
    }

    let mut slot = vec![0usize; total];
    for layer in &layers {
        set_slots(layer, &mut slot);
    }
    for sweep in 0..SWEEPS {
        let downward = sweep % 2 == 0;
        let ranks: Vec<usize> = if downward {
            (1..layer_count).collect()
        } else {
            (0..layer_count.saturating_sub(1)).rev().collect()
        };
        for r in ranks {
            let adjacency = if downward { &up } else { &down };
            let mut keyed: Vec<(f64, usize, usize)> = layers[r]
                .iter()
                .enumerate()
                .map(|(i, &node)| {
                    let neighbors = &adjacency[node];
                    let key = if neighbors.is_empty() {
                        i as f64
                    } else {
                        neighbors.iter().map(|&n| slot[n] as f64).sum::<f64>() / neighbors.len() as f64
                    };
                    (key, i, node)
                })
                .collect();
            keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            layers[r] = keyed.into_iter().map(|(_, _, node)| node).collect();
            set_slots(&layers[r], &mut slot);
        }
    }

    let crossings = count_crossings(&segments, &node_rank, &slot);
    let edge_paths = chains
        .iter()
        .map(|chain| chain.iter().map(|&n| (node_rank[n], slot[n])).collect())
        .collect();
    Ok(Layout {
        rank: node_rank[..nodes].to_vec(),
        slot: slot[..nodes].to_vec(),
        edge_paths,
        crossings,
    })
}

fn set_slots(layer: &[usize], slot: &mut [usize]) {
    for (index, &node) in layer.iter().enumerate() {
        slot[node] = index;
    }
}

/// Longest-path ranking: a node sits one rank below its deepest predecessor. Fails on cycles.
fn assign_ranks(nodes: usize, edges: &[(usize, usize)]) -> Result<Vec<usize>, LayoutError> {
    let mut indegree = vec![0usize; nodes];
    let mut successors = vec![Vec::new(); nodes];
    for &(u, v) in edges {
        indegree[v] += 1;
        successors[u].push(v);
    }
    let mut queue: VecDeque<usize> = (0..nodes).filter(|&n| indegree[n] == 0).collect();
    let mut rank = vec![0usize; nodes];
    let mut visited = 0;
    while let Some(u) = queue.pop_front() {
        visited += 1;
        for &v in &successors[u] {
            rank[v] = rank[v].max(rank[u] + 1);
            indegree[v] -= 1;
            if indegree[v] == 0 {
                queue.push_back(v);
            }
        }
    }
    if visited != nodes {
        return Err(LayoutError::Cycle);
    }
    Ok(rank)
}

/// Two segments between the same pair of ranks cross when their tails and heads are ordered oppositely.
fn count_crossings(segments: &[(usize, usize)], node_rank: &[usize], slot: &[usize]) -> usize {
    let mut count = 0;
    for (i, &(a, b)) in segments.iter().enumerate() {
        for &(c, d) in &segments[i + 1..] {
            if node_rank[a] != node_rank[c] || node_rank[b] != node_rank[d] {
                continue;
            }
            let tails = slot[a].cmp(&slot[c]);
            let heads = slot[b].cmp(&slot[d]);
            if tails != Ordering::Equal && heads != Ordering::Equal && tails != heads {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_ranks_one_below_its_predecessor() {
        let layout = layered(3, &[(0, 1), (1, 2)]).unwrap();
        assert_eq!(layout.rank, vec![0, 1, 2]);
        assert_eq!(layout.crossings, 0);
    }

    #[test]
    fn a_long_edge_is_routed_through_a_dummy_node_on_each_intermediate_rank() {
        let layout = layered(3, &[(0, 2), (0, 1), (1, 2)]).unwrap();
        let ranks: Vec<usize> = layout.edge_paths[0].iter().map(|&(rank, _)| rank).collect();
        assert_eq!(ranks, vec![0, 1, 2]);
    }

    #[test]
    fn barycenter_reduction_removes_a_crossing_present_in_input_order() {
        let layout = layered(4, &[(0, 3), (1, 2)]).unwrap();
        assert_eq!(layout.crossings, 0);
        assert!(layout.slot[3] < layout.slot[2], "node 3 must move left to uncross the edges");
    }

    #[test]
    fn a_cycle_is_refused() {
        assert_eq!(layered(2, &[(0, 1), (1, 0)]), Err(LayoutError::Cycle));
    }

    #[test]
    fn a_self_loop_is_a_cycle() {
        assert_eq!(layered(1, &[(0, 0)]), Err(LayoutError::Cycle));
    }

    #[test]
    fn an_edge_to_a_missing_node_is_refused() {
        assert_eq!(layered(2, &[(0, 5)]), Err(LayoutError::NodeOutOfRange { node: 5 }));
    }
}
