//! The rebuild: a new packed graph from the old packed graph plus the actions since it was built.
//!
//! It reads only the old packed arrays and the writer's journal (never the live edit structures), so it sees exactly
//! the actions up to the point it started. Nodes are renumbered: deleted nodes are dropped, added nodes join their
//! type, and each type's nodes stay together: packed ones in breadth-first order (see `storage::order`), then added
//! ones.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::collections::IndexVec;
use crate::core::Value;
use crate::core::{link, node};
use crate::storage::graph::Graph;
use crate::storage::keys::{KeyIndex, PrimaryKeys};
use crate::storage::link_type;
use crate::storage::node_type;
use crate::storage::order;
use crate::storage::property::{self, Cell};

/// No node: the old node was deleted.
pub const NONE: node::GlobalId = node::GlobalId(u64::MAX);

/// One edit as the writer journals it. Node numbers are those of the generation the edit was made on; a node added
/// by `AddNode` gets the next number after all earlier ones.
#[derive(Clone, Debug)]
pub enum Op {
    AddNode {
        t: node_type::Id,
        key: Vec<u8>,
    },
    SetProperty {
        node: node::GlobalId,
        property: usize,
        value: Value,
    },
    DeleteNode {
        node: node::GlobalId,
    },
    AddLink {
        r: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    },
    /// Only journaled when a link was found and removed.
    RemoveLink {
        r: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    },
}

pub struct Rebuilt {
    pub graph: Graph,
    /// Old node number → new node number (or `NONE`), for every node the journal knew about.
    pub remap: Vec<node::GlobalId>,
}

pub fn rebuild(old: &Graph, ops: &[(u64, Op)]) -> Rebuilt {
    let base = old.node_count() as u64;
    let types = old.node_types.len();
    let mut added: Vec<(node_type::Id, &[u8])> = Vec::new();
    let mut deleted: FxHashSet<u64> = FxHashSet::default();
    let mut latest: FxHashMap<(u64, usize), &Value> = FxHashMap::default();
    // Per link type: links added in order, and per (from, to) how many links were added minus removed.
    let mut adds: IndexVec<link_type::Id, Vec<(u64, u64)>> =
        IndexVec::from_fn(old.link_types.len(), |_| Vec::new());
    let mut delta: IndexVec<link_type::Id, FxHashMap<(u64, u64), i64>> =
        IndexVec::from_fn(old.link_types.len(), |_| FxHashMap::default());
    for (_, op) in ops {
        match op {
            Op::AddNode { t, key } => added.push((*t, key)),
            Op::SetProperty {
                node,
                property,
                value,
            } => {
                latest.insert((node.0, *property), value);
            }
            Op::DeleteNode { node } => {
                deleted.insert(node.0);
            }
            Op::AddLink { r, from, to } => {
                adds[*r].push((from.0, to.0));
                *delta[*r].entry((from.0, to.0)).or_default() += 1;
            }
            Op::RemoveLink { r, from, to } => *delta[*r].entry((from.0, to.0)).or_default() -= 1,
        }
    }
    let total = base + added.len() as u64;
    let type_of = |g: u64| {
        if g < base {
            old.node_type_mappings.type_of(node::GlobalId(g))
        } else {
            added[(g - base) as usize].0
        }
    };

    // Nodes, type after type; within a type, packed nodes in breadth-first order (linked nodes close together),
    // then the nodes added since, in the order they were added.
    let mut by_type: IndexVec<node_type::Id, Vec<u64>> = IndexVec::from_fn(types, |_| Vec::new());
    for g in order::breadth_first(old).into_iter().chain(base..total) {
        if !deleted.contains(&g) {
            by_type[type_of(g)].push(g);
        }
    }
    let alive: usize = by_type.iter().map(Vec::len).sum();
    let mut map = node_type::Map::new(types);
    map.reserve(alive);
    let mut keys = PrimaryKeys::new();
    let mut key_index = KeyIndex::with_capacity(alive);
    let mut remap = vec![NONE; total as usize];
    for (t, nodes) in by_type.iter_ids() {
        for &g in nodes {
            let (ng, _) = map.push(t);
            remap[g as usize] = ng;
            let key = if g < base {
                old.keys.get(node::GlobalId(g))
            } else {
                added[(g - base) as usize].1
            };
            keys.push(key);
            key_index.insert(t, key, ng);
        }
    }

    // Properties: the latest change if there is one, else the packed value.
    let node_types = old
        .node_types
        .iter_ids()
        .map(|(t, nt)| {
            let properties = nt
                .properties
                .iter()
                .enumerate()
                .map(|(pi, p)| {
                    let cells: Vec<Option<Cell>> = by_type[t]
                        .iter()
                        .map(|&g| match latest.get(&(g, pi)) {
                            Some(v) => Some(property::cell_from_value(p.kind, v)),
                            None if g < base => {
                                property::cell(p, old.node_type_mappings.get(node::GlobalId(g)).1.0 as u64)
                            }
                            None => None,
                        })
                        .collect();
                    property::build_cells(&p.name, p.kind, &cells)
                })
                .collect();
            node_type::Storage {
                name: nt.name.clone(),
                properties,
            }
        })
        .collect();

    // Link types: packed links minus removed ones, then added links still standing; links to deleted nodes dropped.
    // The journal and `remap` are in global numbers; the tables are in positions within node types.
    let old_map = &old.node_type_mappings;
    let position = |g: node::GlobalId| map.get(g).1;
    let link_types = old
        .link_types
        .iter_ids()
        .map(|(ri, rel)| {
            let mut next_link = 0u32;
            let delta = &mut delta[ri];
            let (from_first, to_first) = (old_map.first(rel.from_type).0, old_map.first(rel.to_type).0);
            let mut triples = Vec::with_capacity(rel.forward.len() + adds[ri].len());
            let mut keep = |u: u64, v: u64, triples: &mut Vec<(node::Id, node::Id, link::Id)>| {
                let (a, b) = (remap[u as usize], remap[v as usize]);
                if a != NONE && b != NONE {
                    triples.push((position(a), position(b), link::Id(next_link)));
                    next_link += 1;
                }
            };
            for row in 0..rel.forward.rows() as u32 {
                let u = from_first + row as u64;
                for &v in rel.forward.neighbours(node::Id(row)) {
                    let v = to_first + v.0 as u64;
                    if let Some(d) = delta.get_mut(&(u, v)) {
                        if *d < 0 {
                            *d += 1;
                            continue;
                        }
                    }
                    keep(u, v, &mut triples);
                }
            }
            for &(from, to) in &adds[ri] {
                let d = delta.get_mut(&(from, to)).unwrap();
                if *d > 0 {
                    *d -= 1;
                    keep(from, to, &mut triples);
                }
            }
            link_type::Storage::build(
                &rel.name,
                rel.from_type,
                rel.to_type,
                map.count(rel.from_type) as usize,
                map.count(rel.to_type) as usize,
                &triples,
            )
        })
        .collect();

    Rebuilt {
        graph: Graph {
            node_types,
            node_type_mappings: map,
            link_types,
            keys,
            key_index,
        },
        remap,
    }
}
