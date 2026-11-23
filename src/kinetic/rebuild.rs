//! The rebuild: a new packed graph from the old packed graph plus the actions since it was built.
//!
//! It reads only the old packed arrays and the writer's journal (never the draft edit structures), so it sees exactly
//! the actions up to the point it started. In order:
//!
//!   1. Read the journal: added and deleted nodes, the latest value of each changed property, and per link type the
//!      links added and how many of each were added minus removed.
//!   2. Lay out the nodes: deleted ones dropped, each type's nodes together, packed ones in breadth-first order (see
//!      `storage::order`) then added ones. This gives every node its new number (`remap`), and the new keys.
//!   3. Rebuild each property column: the latest change if there is one, else the packed value.
//!   4. Rebuild each link type's tables: packed links not removed, then added links still standing; links to deleted
//!      nodes dropped.

use rustc_hash::{FxHashMap, FxHashSet};

use crate::collections::IndexVec;
use crate::core::Value;
use crate::core::{link, node};
use crate::storage::graph::Graph;
use crate::storage::keys::{KeyIndex, PrimaryKeys};
use crate::storage::link_type;
use crate::storage::node_type;
use crate::storage::order;
use crate::storage::property::{self, Cell, Property};

/// No node: the old node was deleted.
pub const NONE: node::GlobalId = node::GlobalId(u64::MAX);


/// One edit as the writer journals it. Node numbers are those of the generation the edit was made on; a node added
/// by `AddNode` gets the next number after all earlier ones.
#[derive(Clone, Debug)]
pub enum Op {
    AddNode {
        node_type: node_type::Id,
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
        link_type: link_type::Id,
        from: node::GlobalId,
        to: node::GlobalId,
    },
    /// Only journaled when a link was found and removed.
    RemoveLink {
        link_type: link_type::Id,
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
    let mut journal = Journal::read(old, ops);

    let layout = Layout::of(old, &journal);

    let node_types = old
        .node_types
        .iter_ids()
        .map(|(node_type, storage)| node_type::Storage {
            name: storage.name.clone(),
            primary_key: storage.primary_key.clone(),
            properties: properties_of(old, &journal, &layout, node_type),
        })
        .collect();

    let link_types = old
        .link_types
        .iter_ids()
        .map(|(link_type, storage)| link_type_of(old, &mut journal, &layout, link_type, storage))
        .collect();

    Rebuilt {
        graph: Graph {
            node_types,
            node_type_mappings: layout.map,
            link_types,
            keys: layout.keys,
            key_index: layout.key_index,
        },
        remap: layout.remap,
    }
}


// ---------------------------------------------------------------- 1. the journal


/// What the actions since the last rebuild add up to, in old node numbers.
struct Journal<'o> {
    /// Nodes in the old packed graph; added nodes are numbered from here.
    base: u64,
    added: Vec<(node_type::Id, &'o [u8])>,
    deleted: FxHashSet<u64>,
    /// (node, property) → its latest value.
    latest: FxHashMap<(u64, usize), &'o Value>,
    links: IndexVec<link_type::Id, LinkEdits>,
}


#[derive(Default)]
struct LinkEdits {
    /// Links added, in order, as (from, to).
    added: Vec<(u64, u64)>,
    /// Per (from, to): links added minus links removed.
    balance: FxHashMap<(u64, u64), i64>,
}


impl<'o> Journal<'o> {
    fn read(old: &Graph, ops: &'o [(u64, Op)]) -> Self {
        let mut journal = Journal {
            base: old.node_count() as u64,
            added: Vec::new(),
            deleted: FxHashSet::default(),
            latest: FxHashMap::default(),
            links: IndexVec::from_fn(old.link_types.len(), |_| LinkEdits::default()),
        };

        for (_, op) in ops {
            match op {
                Op::AddNode { node_type, key } => journal.added.push((*node_type, key)),

                Op::SetProperty {
                    node,
                    property,
                    value,
                } => {
                    journal.latest.insert((node.0, *property), value);
                }

                Op::DeleteNode { node } => {
                    journal.deleted.insert(node.0);
                }

                Op::AddLink {
                    link_type,
                    from,
                    to,
                } => {
                    let edits = &mut journal.links[*link_type];

                    edits.added.push((from.0, to.0));
                    *edits.balance.entry((from.0, to.0)).or_default() += 1;
                }

                Op::RemoveLink {
                    link_type,
                    from,
                    to,
                } => {
                    *journal.links[*link_type]
                        .balance
                        .entry((from.0, to.0))
                        .or_default() -= 1;
                }
            }
        }

        journal
    }


    /// Every node number the journal knows: packed ones, then added ones.
    fn total(&self) -> u64 {
        self.base + self.added.len() as u64
    }


    fn type_of(&self, old: &Graph, g: u64) -> node_type::Id {
        if g < self.base {
            return old.node_type_mappings.type_of(node::GlobalId(g));
        }

        self.added[(g - self.base) as usize].0
    }


    fn key_of<'k>(&'k self, old: &'k Graph, g: u64) -> &'k [u8] {
        if g < self.base {
            return old.keys.get(node::GlobalId(g));
        }

        self.added[(g - self.base) as usize].1
    }
}


// ---------------------------------------------------------------- 2. the node layout


/// The new node numbering and everything keyed by it.
struct Layout {
    /// Per node type, its surviving nodes (old numbers) in their new order.
    by_type: IndexVec<node_type::Id, Vec<u64>>,
    map: node_type::Map,
    keys: PrimaryKeys,
    key_index: KeyIndex,
    remap: Vec<node::GlobalId>,
}


impl Layout {
    fn of(old: &Graph, journal: &Journal) -> Self {
        let types = old.node_types.len();
        let mut by_type: IndexVec<node_type::Id, Vec<u64>> =
            IndexVec::from_fn(types, |_| Vec::new());

        let packed_in_order = order::breadth_first(old).into_iter();
        let added = journal.base..journal.total();

        for g in packed_in_order.chain(added) {
            if !journal.deleted.contains(&g) {
                by_type[journal.type_of(old, g)].push(g);
            }
        }

        let alive: usize = by_type.iter().map(Vec::len).sum();

        let mut map = node_type::Map::new(types);
        let mut keys = PrimaryKeys::new();
        let mut key_index = KeyIndex::with_capacity(alive);
        let mut remap = vec![NONE; journal.total() as usize];

        map.reserve(alive);

        for (node_type, nodes) in by_type.iter_ids() {
            for &g in nodes {
                let (new, _) = map.push(node_type);
                let key = journal.key_of(old, g);

                remap[g as usize] = new;
                keys.push(key);
                key_index.insert(node_type, key, new);
            }
        }

        Layout {
            by_type,
            map,
            keys,
            key_index,
            remap,
        }
    }


    /// A node's position within its type, from its new number.
    fn position(&self, new: node::GlobalId) -> node::Id {
        self.map.get(new).1
    }
}


// ---------------------------------------------------------------- 3. property columns


fn properties_of(
    old: &Graph,
    journal: &Journal,
    layout: &Layout,
    node_type: node_type::Id,
) -> Vec<Property> {
    old.node_types[node_type]
        .properties
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let cells: Vec<Option<Cell>> = layout.by_type[node_type]
                .iter()
                .map(|&g| cell_of(old, journal, column, index, g))
                .collect();

            property::build_cells(&column.name, column.kind, &cells)
        })
        .collect()
}


/// A node's value of one property: its latest change if there is one, else its packed value.
fn cell_of(
    old: &Graph,
    journal: &Journal,
    column: &Property,
    index: usize,
    g: u64,
) -> Option<Cell> {
    if let Some(value) = journal.latest.get(&(g, index)) {
        return Some(property::cell_from_value(column.kind, value));
    }

    if g >= journal.base {
        return None;
    }

    let position = old.node_type_mappings.get(node::GlobalId(g)).1;

    property::cell(column, position.0 as u64)
}


// ---------------------------------------------------------------- 4. link tables


/// One link type's new tables. Link numbers restart from 0 for each link type.
fn link_type_of(
    old: &Graph,
    journal: &mut Journal,
    layout: &Layout,
    link_type: link_type::Id,
    storage: &link_type::Storage,
) -> link_type::Storage {
    let old_map = &old.node_type_mappings;
    let from_first = old_map.first(storage.from_type).0;
    let to_first = old_map.first(storage.to_type).0;

    let edits = &mut journal.links[link_type];
    let mut triples = Vec::with_capacity(storage.forward.len() + edits.added.len());

    // Keeps a link if both ends survive, in new positions.
    let mut keep = |from: u64, to: u64| {
        let new_from = layout.remap[from as usize];
        let new_to = layout.remap[to as usize];

        if new_from != NONE && new_to != NONE {
            let number = link::Id(triples.len() as u32);

            triples.push((layout.position(new_from), layout.position(new_to), number));
        }
    };

    // Packed links, unless removed: each removal cancels one packed copy of the link.
    for row in 0..storage.forward.rows() as u32 {
        let from = from_first + row as u64;

        for &to in storage.forward.neighbours(node::Id(row)) {
            let to = to_first + to.0 as u64;

            if let Some(balance) = edits.balance.get_mut(&(from, to))
                && *balance < 0
            {
                *balance += 1;
                continue;
            }

            keep(from, to);
        }
    }

    // Added links, as many of each as the balance says still stand.
    for &(from, to) in &edits.added {
        let balance = edits
            .balance
            .get_mut(&(from, to))
            .expect("every added link has a balance");

        if *balance > 0 {
            *balance -= 1;
            keep(from, to);
        }
    }

    link_type::Storage::build(
        &storage.name,
        storage.from_type,
        storage.to_type,
        layout.map.count(storage.from_type) as usize,
        layout.map.count(storage.to_type) as usize,
        &triples,
    )
}
