//! The order nodes are laid out in: linked nodes get numbers close together, so a walk that reaches one finds the
//! others' rows in the same cache lines.

use crate::collections::{Bitset, IndexVec};
use crate::core::node;
use crate::storage::graph::Graph;
use crate::storage::link_type::Table;
use crate::storage::node_type;

/// Every packed node, by global number, in breadth-first order over all link types in both directions. Each search
/// starts from the highest-degree node not reached yet, and a node's neighbours follow it in the order its tables
/// hold them.
pub fn breadth_first(graph: &Graph) -> Vec<u64> {
    let n = graph.node_count();
    let map = &graph.node_type_mappings;
    // Per node type: each table a node of that type has a row in, with the first global number of its rows' type
    // and of the type it points to.
    let mut tables: IndexVec<node_type::Id, Vec<(&Table, u64, u64)>> =
        IndexVec::from_fn(graph.node_types.len(), |_| Vec::new());
    for rel in &graph.link_types {
        let (from, to) = (map.first(rel.from_type).0, map.first(rel.to_type).0);
        tables[rel.from_type].push((&rel.forward, from, to));
        tables[rel.to_type].push((&rel.backward, to, from));
    }
    let row = |u: u64, row_first: u64| node::Id((u - row_first) as u32);

    let degrees: Vec<u64> = (0..n as u64)
        .map(|u| {
            tables[map.type_of(node::GlobalId(u))]
                .iter()
                .map(|&(t, row_first, _)| t.degree(row(u, row_first)))
                .sum()
        })
        .collect();
    let mut seeds: Vec<u32> = (0..n as u32).collect();
    seeds.sort_unstable_by_key(|&u| std::cmp::Reverse(degrees[u as usize]));
    drop(degrees);

    let mut seen = Bitset::new(n);
    let mut order: Vec<u64> = Vec::with_capacity(n);
    let mut head = 0;
    for s in seeds {
        let s = s as u64;
        if seen.get(s as usize) {
            continue;
        }
        seen.set(s as usize);
        order.push(s);
        while head < order.len() {
            let u = order[head];
            head += 1;
            for &(t, row_first, other_first) in &tables[map.type_of(node::GlobalId(u))] {
                for &v in t.neighbours(row(u, row_first)) {
                    let v = other_first + v.0 as u64;
                    if !seen.get(v as usize) {
                        seen.set(v as usize);
                        order.push(v);
                    }
                }
            }
        }
    }
    order
}
